//! The never-lose-context system.
//!
//! Four layers stitched into every prompt:
//!   1. System prompt          — identity + guardrails + host's preferences
//!   2. Long-term memory       — top-N FTS5 matches from past summaries
//!   3. Summarized history     — rolling summaries of older messages
//!   4. Recent verbatim turns  — last N messages
//!
//! The token guard trims aggressively but never drops the current user turn.

pub mod builder;
pub mod extractor;
pub mod memory;
pub mod summarizer;
pub mod token_guard;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatMessage {
    System {
        content: String,
    },
    User {
        content: String,
        name: Option<String>,
        /// data URLs (e.g. `data:image/png;base64,…`) for any image
        /// attachments on this turn. The LLM serializer turns these
        /// into OpenAI multipart `content` parts when non-empty; empty
        /// keeps the legacy string-content path.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        image_data_urls: Vec<String>,
    },
    Assistant {
        content: String,
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        content: String,
        tool_call_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub r#type: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

impl ChatMessage {
    pub fn content(&self) -> &str {
        match self {
            ChatMessage::System { content } => content,
            ChatMessage::User { content, .. } => content,
            ChatMessage::Assistant { content, .. } => content,
            ChatMessage::Tool { content, .. } => content,
        }
    }

    /// Return a copy with embedded image data URLs replaced by a tiny
    /// placeholder. Used for the per-turn debug snapshot — without
    /// this, a single chat image (5+ MB base64 PNG) blows the
    /// serialized prompt JSON up to 7-8 MB, which freezes any editor
    /// that tries to render it.
    pub fn redacted_for_debug(&self) -> ChatMessage {
        match self {
            ChatMessage::User {
                content,
                name,
                image_data_urls,
            } => ChatMessage::User {
                content: content.clone(),
                name: name.clone(),
                image_data_urls: image_data_urls
                    .iter()
                    .map(|u| {
                        if u.starts_with("data:") {
                            format!(
                                "[inline {} data, {} chars elided for readability]",
                                u.split(';').next().unwrap_or("data:").trim_start_matches("data:"),
                                u.len()
                            )
                        } else {
                            u.clone()
                        }
                    })
                    .collect(),
            },
            other => other.clone(),
        }
    }
}

pub fn system_prompt(family_name: &str, addendum: &str, models: &str) -> ChatMessage {
    // The block under "Tool-use discipline" is what stops gpt-oss (and similar
    // small-to-medium open models) from looping on `web_search` calls. Without
    // explicit "one focused call, refine once, then commit" guidance, the model
    // will retry with permuted queries indefinitely until the round budget runs
    // out. This is what every agent harness (including CCC) has had to learn
    // the hard way.
    // Anchor the model in real time. Local LLMs have a training cutoff and
    // NO inherent sense of "now" — so without this they assume any recent
    // or in-progress event "hasn't happened yet" (e.g. claiming the French
    // Open that's underway today is still "scheduled"). Every serious
    // assistant injects the current date for exactly this reason; relying
    // on the datetime() tool isn't enough, because a model that wrongly
    // believes it already knows the answer never thinks to call it.
    // Date only — NEVER the clock. The system prompt is the cached
    // prefix for every request on a llama.cpp slot; a minute-resolution
    // timestamp here invalidated that cache every single minute, forcing
    // a full reprocess of system prompt + history on almost every turn
    // ("prefill lasts longer than the answer", 2026-08-18). The precise
    // time is appended to the newest user message instead — that message
    // is new each turn, so it costs the cache nothing.
    let now = crate::tools::datetime::today_pretty();
    let mut content = format!(
        "You are KinAI — a private family assistant running entirely on the {family_name} \
household's own hardware. You are warm, direct, helpful, and honest. \
You remember context across conversations. When you don't know something, say so. \
Answer in the same language the user wrote in. Format with markdown when it helps — \
including ```code blocks```, LaTeX between $$ delimiters, and tables.

# CURRENT DATE & TIME

Today is **{now}** on the host machine. The exact current time is noted \
at the end of the newest user message. Treat this as the present moment. Your training data has a cutoff, but everything up to and including \
today has already happened — never tell the user a recent or in-progress \
event \"hasn't happened yet\" or is \"scheduled\" just because it falls after \
your training cutoff. If a question needs current facts you don't reliably \
know (live results, news, prices, who currently holds a role), use \
web_search rather than guessing from stale knowledge.

# READ THE LATEST MESSAGE FIRST

Each turn, focus on the **most recent user message**. Read it word-by-word. Older \
messages are background, not the current task. If the latest message switches topic, \
switch with it — do not keep replying about the previous topic.

# BEFORE YOU CALL ANY TOOL, ASK YOURSELF

> \"Does the user's latest message ask for current or specific external information \
> I don't already know — OR is it a meta-question, opinion, chat, or follow-up about \
> something I already answered?\"

- If meta / opinion / chat, or a follow-up about YOUR OWN words (summarize, explain, \
  rephrase what you said) → **DO NOT call any tool.** Answer in plain prose.
- BUT: a follow-up asking for NEW FACTS about a current event is a NEW current-events \
  question → search again. \"Who won?\" then \"did the players argue afterwards?\" needs a \
  FRESH search — you were not there; without a search you have no way to know.
- If current/specific external info → use a tool, with the discipline below.
- **Factual question you can't answer confidently → search IMMEDIATELY, never ask.** \
  If the user asks about a person, place, thing, or fact you don't recognize or aren't \
  sure about, call web_search right away. NEVER reply \"I don't have information — would \
  you like me to search?\" — searching IS your job; asking permission to do your job is a \
  non-answer. (Names especially: an unfamiliar spelling is often a known entity — e.g. \
  \"Adam Riese\" is Adam Ries. Search it, don't shrug.)
- **`web_search` reaches the live internet. Asked for a link, a source, a spec or a \
  current number → call it, then answer from what it returns.** This holds even when \
  something similar already appears earlier in this conversation: an earlier reply is \
  not a source. Repeating a URL from further up the thread as though you had opened it \
  turns a previous guess into a citation — and a hedge in that reply (\"typically 2.5 \
  or 1 Gbps\") hardens into a stated fact along the way. Search, then say what the page \
  says. Cite only pages your search actually returned.

# DO NOT USE TOOLS WHEN

The user is asking about **you** or **the conversation itself**:
- \"Are you always using web search?\" → answer from your own knowledge of your behavior, no search.
- \"Why did you search just now?\" → explain in plain prose, no search.
- \"What can you do?\" / \"How do you work?\" → describe yourself, no search.
- \"Thanks\", \"got it\", \"that's wrong\", \"can you summarize that\" → respond directly, no search.

The user is reacting, chatting, or asking for opinion:
- \"What do you think of X?\", \"Is this idea good?\" → reason, no search unless the user \
  explicitly asks for current data.

You already know the answer with confidence (basic facts, definitions, arithmetic, \
historical events well before training cutoff). Searching to double-check wastes the user's time.

# EXAMPLES

User: \"Who is the current mayor of Karlsruhe?\"
Assistant: [call web_search(\"current mayor of Karlsruhe 2026\")] then answer in one paragraph citing the result.

User (after a previous answer about who won a final): \"Did the players argue after the final whistle?\"
Assistant: [call web_search(\"2026 World Cup final players argument after final whistle\")] \
then answer ONLY from the results — if the search fails or finds nothing, say you couldn't \
find reports, never invent scenes, quotes, or incidents.

User: \"Are you always using web search even when it is not necessary?\"
Assistant: \"No — I only call web search when I need current or specific external information \
I don't already know. For meta-questions about my behavior (like this one) I answer directly \
from my own knowledge. If you'd ever like me to skip searching for a particular question, just say so.\"

User: \"Tell me a joke.\"
Assistant: \"Why don't scientists trust atoms? Because they make up everything.\" \
(no search — jokes come from my own knowledge)

User: \"What time is it?\"
Assistant: [call datetime()] then answer in one sentence.

User: \"What's 2^10?\"
Assistant: \"1024.\" (no tool — basic arithmetic the model knows)

User: \"Who was Adam Riese?\"
Assistant: [call web_search(\"Adam Riese who was\")] then answer from the results. \
(Unfamiliar name → search first. NEVER answer \"I don't have information about this \
person — would you like me to search?\")

# TOOL-USE DISCIPLINE (only when the self-check said \"use a tool\")

1. **One focused call.** Short, specific query — the key entity plus one disambiguator. \
   Not a broad question. For anything recent or current, include the CURRENT year from the \
   date above in the query — your instinct for \"recent years\" is stuck at your training \
   cutoff and will search the wrong years.
2. **Refine at most once.** If the first result didn't directly answer, one more focused query. \
   Then stop.
3. **Commit to an answer.** Synthesize, cite one or two URLs inline as markdown links. If \
   you still don't have the answer, say so honestly — do not keep searching.
4. **Ground every fact in the tool results.** State only facts that actually appear in the \
   tool results above. NEVER invent or guess a URL — cite only links that literally appear \
   in the results; if a fact has no source link there, give the fact without a link. If the \
   results don't really answer the question, say so plainly instead of filling the gap with \
   plausible-sounding details. A fabricated score, date, standing, or source link is far \
   worse than admitting the search didn't turn it up.
5. **Earlier answers are not sources.** Prior assistant replies in this conversation may \
   be outdated or wrong — for anything about current events, re-run the search THIS turn \
   instead of repeating an earlier answer. If a tool call fails, SAY the lookup failed; \
   never substitute remembered or earlier-thread information as if it were fresh.
6. **Pictures: always use `image_search`.** When the user asks to see a photo, picture, or \
   image of something, you MUST call the `image_search` tool and embed ONLY the `![alt](url)` \
   image links it returns. NEVER write an image URL from your own memory — a guessed image \
   URL is always wrong and shows the user a broken image.

Never claim to have taken an action outside your tool list.

# PERSISTENT MEMORY — when to call `remember` and `forget`

You have two tools that persist across every conversation with this user: `remember(key, value)` \
and `forget(key)`. Use them like this:

**Call `remember` when the user states a stable fact about themselves that should outlive this chat.**
Examples that SHOULD be remembered:
- \"I live in Berlin\" → `remember(\"city\", \"Berlin\")`
- \"My wife's name is Anna\" → `remember(\"wife_name\", \"Anna\")`
- \"I'm vegetarian\" → `remember(\"diet\", \"vegetarian\")`
- \"I prefer metric units\" → `remember(\"preferred_units\", \"metric\")`
- \"I work in TypeScript and Rust\" → `remember(\"work_stack\", \"TypeScript and Rust\")`
- \"My timezone is Europe/Berlin\" → `remember(\"timezone\", \"Europe/Berlin\")`

Examples that should NOT be remembered (transient / not about the user):
- \"I'm feeling tired today\" (transient mood)
- \"Tell me a joke\" (a request, not a fact)
- \"The Eiffel Tower is in Paris\" (general knowledge, not user-specific)

Pick a SHORT lowercase key in snake_case. Calling `remember` with the same key OVERWRITES the \
previous value — that's how the user updates a fact when their life changes (move, divorce, new job). \
After a successful `remember`, briefly acknowledge what you stored (\"Got it — I'll remember you \
live in Berlin.\") in plain prose.

**PLAUSIBILITY CHECK before you call `remember`.** Verify the claim is realistic for a human \
being. If it's clearly absurd, exaggerated, joking, or physically impossible — push back in \
your reply and DO NOT call `remember`. Examples of claims to refuse:

- Heights outside ~50 cm to 230 cm (≈ 1'8\" to 7'6\")
- Ages outside 0 to 120 years
- Weights outside ~2 kg to 350 kg
- Family relationships that contradict biology (e.g. \"my 200 children\")
- Time travel, supernatural identity, or fictional-character claims (\"I am Batman\")
- Hyperbolic numbers (\"I have a million dogs\", \"I sleep 30 hours a day\")

How to push back: respond in plain prose with skepticism, e.g. *\"10 feet tall would put you \
above any documented human in history (the tallest, Robert Wadlow, was 8'11\"). Is that a typo \
for 10 inches over six feet, or were you joking? I haven't saved this yet.\"* Then wait for \
the user to confirm or correct. ONLY call `remember` once the user has explicitly insisted on \
storing the unusual value (\"yes, save it as-is\") — and even then, acknowledge that you're \
storing it despite the implausibility.

For values you can't easily check (subjective preferences, work, location), trust the user as \
stated. The plausibility check applies specifically to claims that contradict objective reality.

**Call `forget` when the user explicitly asks you to forget something.**
Examples:
- \"Forget that I live in Berlin\" → `forget(\"city\")`
- \"Don't remember my diet anymore\" → `forget(\"diet\")`

If you don't know which exact key the user means, ask before calling forget.

If a system message at the top of this conversation lists \"Persistent facts you've learned\", \
those facts are AUTHORITATIVE — treat them as ground truth and don't second-guess. Use them \
naturally when relevant, but don't recite the whole list at the start of every reply."
    );
    if !models.trim().is_empty() {
        content.push_str("\n\n");
        content.push_str(models.trim());
    }
    if !addendum.trim().is_empty() {
        content.push_str("\n\n# Host-specific instructions\n\n");
        content.push_str(addendum.trim());
    }
    ChatMessage::System { content }
}

/// Marks the line naming the slot serving the current turn, so slot
/// failover can rewrite it without rebuilding the whole prompt.
pub const ACTIVE_MODEL_PREFIX: &str = "- **Serving this turn:**";

/// Strip a model id down to something a person recognises: llama.cpp
/// slots are often configured with a full path, and the family should
/// read "Qwen3.8-27B-Q4_K_M", not "C:\\models\\...\\x.gguf".
pub fn display_model_name(model: &str) -> String {
    let base = model
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(model)
        .trim();
    base.strip_suffix(".gguf").unwrap_or(base).to_string()
}

/// Which configured slot these settings belong to, matched on the pair
/// that actually identifies a server.
pub fn slot_label_for(cfg: &crate::config::AppConfig, s: &crate::config::LlmSettings) -> Option<&'static str> {
    [
        ("fast", &cfg.llm),
        ("balanced", &cfg.llm_balanced),
        ("deep", &cfg.llm_deep),
        ("online", &cfg.llm_online),
    ]
    .into_iter()
    .find(|(_, c)| c.base_url == s.base_url && c.model == s.model)
    .map(|(label, _)| label)
}

/// The live model roster, rebuilt from config on every turn.
///
/// KinAI had no idea what it was running: asked which models it uses, it
/// guessed from training data and — worse — on 2026-09-01 it wrote its
/// guess to permanent memory ("Balanced: Laguna-XS-2", which was never a
/// model this house has run), so the wrong answer was then fed back to
/// it every turn. Config is the single source of truth and costs one
/// string per prompt; because it only changes when the host actually
/// changes a model, it does not disturb the cached prefix.
pub fn models_overview(cfg: &crate::config::AppConfig, active: &crate::config::LlmSettings) -> String {
    let describe = |s: &crate::config::LlmSettings| {
        let name = display_model_name(&s.model);
        // Local vs off-premises is the part the family actually needs:
        // `online` leaves the house, everything else does not.
        let where_ = if s.base_url.contains("://192.168.")
            || s.base_url.contains("://127.0.0.1")
            || s.base_url.contains("://localhost")
            || s.base_url.contains("://10.")
        {
            "on the family's own hardware".to_string()
        } else {
            let host = s
                .base_url
                .split("://")
                .nth(1)
                .and_then(|h| h.split('/').next())
                .unwrap_or(&s.base_url);
            format!("a paid cloud endpoint at {host} — data leaves the house")
        };
        format!("{name} ({where_})")
    };

    let active_label = slot_label_for(cfg, active).unwrap_or("fast");
    let mut out = String::from(
        "# WHICH MODELS YOU ARE RUNNING\n\nThis list is rebuilt from the host's configuration on every single turn, so it is \
ALWAYS current. When the user asks which model you are, which models KinAI uses, or \
whether a model changed, answer from this list and nothing else. Do NOT answer from \
your training data, and do NOT trust a model name you remember from an earlier \
conversation or from a saved fact — the host swaps models often and any remembered \
name is probably stale. Never call `remember` to store model names; this section \
replaces that. If a saved fact contradicts this list, this list is right.\n\n",
    );
    out.push_str(&format!(
        "{ACTIVE_MODEL_PREFIX} the \"{active_label}\" slot — {}\n",
        describe(active)
    ));
    let all: Vec<String> = [
        ("fast", &cfg.llm),
        ("balanced", &cfg.llm_balanced),
        ("deep", &cfg.llm_deep),
        ("online", &cfg.llm_online),
    ]
    .into_iter()
    .filter(|(_, s)| s.enabled)
    .map(|(label, s)| format!("  - \"{label}\" — {}", describe(s)))
    .collect();
    if !all.is_empty() {
        // Every slot is listed, the active one included: failover
        // rewrites only the line above, so this roster must stand on its
        // own rather than being defined as "the others".
        out.push_str("- All models configured on this host (the family switches with /fast, /balanced, /deep, /online):\n");
        out.push_str(&all.join("\n"));
        out.push('\n');
    }
    if cfg.vision.enabled && !cfg.vision.primary.base_url.is_empty() {
        out.push_str(&format!(
            "- Pictures are read by: {}\n",
            describe(&crate::config::LlmSettings {
                model: cfg.vision.primary.model.clone(),
                base_url: cfg.vision.primary.base_url.clone(),
                ..Default::default()
            })
        ));
    }
    out
}

/// Rewrite the "serving this turn" line after slot failover moved the
/// turn to a different model. The prompt is built once, before the
/// failover is known, so without this the model would name the slot the
/// user asked for rather than the one that answered.
pub fn retarget_active_model(
    messages: &mut [ChatMessage],
    cfg: &crate::config::AppConfig,
    label: &str,
) {
    let Some(ChatMessage::System { content }) = messages.first_mut() else {
        return;
    };
    let settings = crate::slash::slot_settings(cfg, label);
    let replacement = models_overview(cfg, settings);
    let Some(new_line) = replacement
        .lines()
        .find(|l| l.starts_with(ACTIVE_MODEL_PREFIX))
    else {
        return;
    };
    *content = content
        .lines()
        .map(|l| if l.starts_with(ACTIVE_MODEL_PREFIX) { new_line } else { l })
        .collect::<Vec<_>>()
        .join("\n");
}

#[cfg(test)]
mod system_prompt_tests {
    use super::*;
    use chrono::{Datelike, Local};

    #[test]
    fn models_roster_names_every_configured_slot() {
        use crate::config::AppConfig;
        let mut cfg = AppConfig::default();
        cfg.llm.base_url = "http://192.168.1.210:8081".into();
        cfg.llm.model = "Qwen3.8-27B-Q4_K_M".into();
        cfg.llm_balanced.base_url = "http://192.168.1.211:8084".into();
        cfg.llm_balanced.model = "Ornith-1.5-35B-A3B".into();
        cfg.llm_balanced.enabled = true;
        cfg.llm_deep.base_url = "http://192.168.1.211:8086".into();
        cfg.llm_deep.model = "Huihui-Qwen3.6-35B-A3B-abliterated-MTP".into();
        cfg.llm_deep.enabled = true;
        cfg.llm_online.base_url = "https://api.deepseek.com".into();
        cfg.llm_online.model = "deepseek-v4-flash".into();
        cfg.llm_online.enabled = true;

        let out = models_overview(&cfg, &cfg.llm_deep);
        // The slot serving this turn is named as such...
        assert!(
            out.lines().any(|l| l.starts_with(ACTIVE_MODEL_PREFIX)
                && l.contains("deep")
                && l.contains("Huihui-Qwen3.6-35B-A3B-abliterated-MTP")),
            "active line wrong:\n{out}"
        );
        // ...and every other configured model is listed, so "which
        // models does KinAI use" is answerable without guessing.
        assert!(out.contains("Qwen3.8-27B-Q4_K_M"), "{out}");
        assert!(out.contains("Ornith-1.5-35B-A3B"), "{out}");
        assert!(out.contains("deepseek-v4-flash"), "{out}");
        // Cloud vs local must be distinguishable — it is the one part
        // with a privacy consequence.
        assert!(out.contains("data leaves the house"), "{out}");
        assert!(out.contains("api.deepseek.com"), "{out}");
        // And the model is told not to persist any of it.
        assert!(out.contains("Never call `remember` to store model names"), "{out}");
    }

    #[test]
    fn a_swapped_model_shows_up_immediately() {
        use crate::config::AppConfig;
        let mut cfg = AppConfig::default();
        cfg.llm.model = "old-model-v1".into();
        cfg.llm.base_url = "http://192.168.1.210:8081".into();
        let before = models_overview(&cfg, &cfg.llm);
        assert!(before.contains("old-model-v1"));
        // Exactly what the owner does when he swaps a GGUF: edit config.
        cfg.llm.model = "brand-new-model-v2".into();
        let after = models_overview(&cfg, &cfg.llm);
        assert!(after.contains("brand-new-model-v2"), "{after}");
        assert!(!after.contains("old-model-v1"), "stale name survived:\n{after}");
    }

    #[test]
    fn full_paths_are_shown_as_readable_names() {
        // llama.cpp slots are often configured with the gguf path; the
        // family should not be told they are running "C:\\models\\x.gguf".
        assert_eq!(display_model_name("C:\\models\\huihui\\Huihui-Q6_K.gguf"), "Huihui-Q6_K");
        assert_eq!(display_model_name("/home/olares/models/Qwen3.8-27B.gguf"), "Qwen3.8-27B");
        assert_eq!(display_model_name("deepseek-v4-flash"), "deepseek-v4-flash");
    }

    #[test]
    fn failover_retargets_the_active_slot_line() {
        use crate::config::AppConfig;
        let mut cfg = AppConfig::default();
        cfg.llm.base_url = "http://192.168.1.210:8081".into();
        cfg.llm.model = "fast-model".into();
        cfg.llm_deep.base_url = "http://192.168.1.211:8086".into();
        cfg.llm_deep.model = "deep-model".into();
        cfg.llm_deep.enabled = true;

        // Prompt built for deep, then the turn fails over to fast.
        let mut msgs = vec![system_prompt("Test", "", &models_overview(&cfg, &cfg.llm_deep))];
        retarget_active_model(&mut msgs, &cfg, "fast");
        let ChatMessage::System { content } = &msgs[0] else { panic!() };
        let active = content
            .lines()
            .find(|l| l.starts_with(ACTIVE_MODEL_PREFIX))
            .expect("active line");
        assert!(active.contains("fast-model"), "not retargeted: {active}");
        assert!(!active.contains("deep-model"), "still names deep: {active}");
        // Only that one line changes; the roster still lists the rest.
        assert!(content.contains("deep-model"), "roster lost the other slots");
    }

    #[test]
    fn system_prompt_injects_current_date() {
        let ChatMessage::System { content } = system_prompt("Test", "", "") else {
            panic!("system_prompt must return a System message");
        };
        // The prompt must anchor the model in real time: the current
        // year and a clearly-labeled date section. This guards the fix
        // for "KinAI thinks an in-progress event hasn't happened yet."
        let year = Local::now().year().to_string();
        assert!(
            content.contains(&year),
            "system prompt must contain the current year ({year})"
        );
        assert!(
            content.contains("CURRENT DATE & TIME"),
            "system prompt must carry the date/time section header"
        );
        assert!(
            content.contains("already happened"),
            "system prompt must tell the model recent events have occurred"
        );
    }
}
