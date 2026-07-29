//! Taking the "should I search?" decision away from the model.
//!
//! Field report (2026-07-29): asked for the Bitcoin price, the balanced
//! slot answered "I don't have real-time market data". Told it could look
//! it up, it did — and kept doing it for the rest of that conversation,
//! then "forgot" again the next session.
//!
//! It is not forgetting. With the same system prompt, `Laguna-XS-2.1`
//! searched 8/8 with a clean history and 8/8 after a good answer — but
//! 0/8 once a single capability refusal sat in the conversation. The model
//! imitates its own previous turn, so one bad answer poisons every turn
//! after it, and the user's correction is what breaks the spell.
//!
//! No prompt wording fixes that reliably, because the prompt is not what
//! the model is copying. So for questions that plainly need live data we
//! stop asking: the first round is issued with the search tool as the ONLY
//! tool and `tool_choice: "required"`.
//!
//! Two findings from probing the backends, both load-bearing:
//!
//!   * `tool_choice: {"type":"function","function":{"name":…}}` — the
//!     OpenAI "force this exact function" form — is SILENTLY IGNORED by
//!     llama.cpp. It returns a normal answer with no tool call and no
//!     error, so code trusting it would believe it had a guarantee it
//!     never had. Only the string `"required"` is honoured.
//!   * `"required"` forces *some* tool, not ours. With the full toolset
//!     offered, Laguna answered a price question by calling `datetime`
//!     twice in six tries. Restricting the forced round to the search
//!     tool alone is what makes it deterministic.
//!
//! # Matching discipline
//!
//! Everything here matches on WORD BOUNDARIES, never raw substrings. The
//! first version of this module used `haystack.contains(needle)` and an
//! adversarial review found it firing a metered web search on:
//! "Can you gene**rate** a picture of a dragon?", "Is that accu**rate**?",
//! "I'm so g**rate**ful", "the best time to visit **Stock**holm",
//! "chicken **stock** for broth", "My son **shares** his toys", and — in
//! German — "Ist 7 eine **gerade** Zahl?" ("even number") and "Yoga
//! **Kurs**" ("course", not "exchange rate"). Bare common words are the
//! enemy; prefer a two-word phrase, and require two independent signals
//! before forcing.

/// Lowercase and reduce to space-delimited word tokens, wrapped in spaces
/// so `" word "` tests are word-boundary tests. Unicode-aware, so German
/// umlauts survive as part of their word.
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push(' ');
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for l in ch.to_lowercase() {
                out.push(l);
            }
        } else if !out.ends_with(' ') {
            out.push(' ');
        }
    }
    if !out.ends_with(' ') {
        out.push(' ');
    }
    out
}

/// True when every space-delimited term appears as a whole word/phrase.
fn has(norm: &str, needle: &str) -> bool {
    norm.contains(&format!(" {needle} "))
}

fn any(norm: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| has(norm, n))
}

/// Unambiguous on their own — a phrase this specific is never about
/// anything but live data.
const STRONG: &[&str] = &[
    "bitcoin price", "btc price", "crypto price", "ethereum price",
    "stock price", "share price", "stock market", "market cap",
    "exchange rate", "wechselkurs", "aktienkurs", "bitcoin kurs",
    "weather forecast", "wetterbericht", "flight status",
    "latest news", "news today", "the news", "any news", "breaking news",
    "die nachrichten", "nachrichten heute", "headlines", "schlagzeilen",
    "who won", "wer hat gewonnen", "spielstand", "final score",
];

/// Live subjects that need a partner signal, because each has a common
/// innocent sense ("chicken stock", "Yoga Kurs", "good news!").
const TOPIC: &[&str] = &[
    "news", "weather", "wetter", "temperature", "forecast",
    "score", "scores", "standings", "ergebnis", "ergebnisse", "traffic",
    "stock", "stocks", "aktie", "aktien", "kurs",
    "bitcoin", "btc", "ethereum", "crypto", "dollar", "euro",
];

/// Words that make a question about a *value*. `rate` is deliberately
/// absent — as a bare word it is the verb in "rate my essay", and its
/// only live sense is covered by the `exchange rate` phrase in STRONG.
const VALUE: &[&str] = &[
    "price", "prices", "preis", "preise", "cost", "costs", "kostet",
    "worth", "how much", "wie viel", "wieviel", "quote",
];

/// About *now*. Never sufficient alone: "How are you today?" and "I have
/// a math test this week" both contain one and neither wants a search.
const TEMPORAL: &[&str] = &[
    "right now", "rn", "now", "today", "todays", "tonight", "tomorrow",
    "currently", "current", "latest", "most recent", "recently",
    "this week", "this month", "at the moment", "as of now", "yesterday",
    "heute", "jetzt", "aktuell", "aktuelle", "aktuellen", "derzeit",
    "momentan", "neueste", "gestern", "morgen",
];

/// "What happened / what's going on" — live when paired with a temporal.
const EVENT_STEM: &[&str] = &[
    "what happened", "what happens", "whats happening", "what is happening",
    "going on", "was ist passiert", "was gibt es neues", "what else happened",
];

/// Asking who holds a role is live however it is phrased. Split into
/// question-stem + role-noun rather than fixed phrases, because a fixed
/// "who is the chancellor" misses "who is the **current** chancellor" —
/// the very phrasing in the field report.
const WHO_STEM: &[&str] = &["who is", "who s", "whos", "wer ist", "who won", "who leads", "who runs"];
const ROLE_NOUN: &[&str] = &[
    "president", "chancellor", "prime minister", "ceo", "mayor", "king",
    "queen", "pope", "bundeskanzler", "kanzler", "praesident", "governor",
    "senator", "champion", "world champion",
];

/// Questions about KinAI or about this conversation. Checked before every
/// positive signal: "what did you say today" contains a temporal word but
/// is a question about us, and searching the web for it is nonsense.
const META: &[&str] = &[
    "you say", "you said", "you just", "your last", "your previous", "your answer",
    "your reply", "did you search", "why did you", "what can you do",
    "how do you work", "who are you", "what are you", "your name",
    "summarize that", "summarise that", "explain that", "rephrase",
    "translate that", "what did you", "what did i", "what do you mean",
    "repeat that", "say that again", "was hast du", "was habe ich", "wer bist du",
];

/// The date and time are already injected into every system prompt, and a
/// `datetime` tool exists for them. Forcing a *web* search for "what time
/// is it" would be slower and worse, and the forced round offers only
/// `web_search`, so it would also hide the right tool.
const DATETIME_ONLY: &[&str] = &[
    "what time", "whats the time", "what day", "whats the date", "what is the date",
    "todays date", "what year", "wie viel uhr", "welcher tag", "welches datum",
];

/// The user's typed prose, with attachment text removed.
///
/// `context::builder::format_user` concatenates what the user typed with
/// the full text extracted from any PDF they attached — up to megabytes of
/// it — separated by a blank line. Classifying that whole blob meant a
/// private document could decide to send a query to a third-party search
/// engine on a turn where the user typed nothing but "summarise this".
/// Only the first paragraph is the question, and a typed question is
/// short.
pub fn typed_prose(user_content: &str) -> &str {
    let first = user_content
        .split("\n\n")
        .next()
        .unwrap_or(user_content)
        .trim();
    // A "question" longer than this is pasted material, not a question.
    const MAX: usize = 400;
    if first.len() <= MAX {
        return first;
    }
    match first.char_indices().nth(MAX) {
        Some((idx, _)) => &first[..idx],
        None => first,
    }
}

/// Should KinAI force a web search for this question instead of leaving
/// the decision to the model?
///
/// Requires **two independent signals** (or one unambiguous phrase).
/// Deliberate consequence: some genuinely live questions are not forced —
/// "did the earthquake in Japan happen yesterday" has a temporal word but
/// no live subject this module can recognise without an endless list of
/// event nouns. Those fall back to the model's own judgement, which is the
/// pre-existing behaviour and is usually right. Forcing is a floor under
/// the clear cases, not a replacement for the model deciding.
pub fn needs_live_data(question: &str) -> bool {
    let q = normalize(typed_prose(question));
    let words = q.split_whitespace().count();
    if words == 0 || question.trim_start().starts_with('/') {
        return false;
    }
    // Vetoes first — each of these contains signals that would otherwise fire.
    if any(&q, META) || any(&q, DATETIME_ONLY) {
        return false;
    }
    if any(&q, STRONG) || (any(&q, WHO_STEM) && any(&q, ROLE_NOUN)) {
        return true;
    }
    if words < 2 {
        return false;
    }
    let temporal = any(&q, TEMPORAL);
    let topic = any(&q, TOPIC);
    let value = any(&q, VALUE);
    // Two signals, in any of the combinations that mean "live":
    //   value + topic     "what does bitcoin cost"
    //   topic + temporal  "weather today", "news today"
    //   value + temporal  "the price right now"
    //   event + temporal  "what happened today"
    (value && topic)
        || (topic && temporal)
        || (value && temporal)
        || (any(&q, EVENT_STEM) && temporal)
}

/// Phrasings of "I have no web access" seen in the field, across both
/// slots and both languages the family uses.
///
/// This list has been wrong twice: an earlier version missed "the ability
/// to browse the web" and "don't have live web access", which let a
/// regression score as a clean pass. When adding a phrasing, add a test.
const DENIAL_MARKERS: &[&str] = &[
    "real time", "realtime", "live access", "live web", "live data",
    "live market", "live financial", "live market data",
    "browse the web", "browsing the web", "browse the internet",
    "access to the internet", "access the internet", "internet access",
    "live web browser", "fetch a link", "training data only",
    "knowledge cutoff", "training cutoff", "echtzeit", "echtzeitdaten",
    "keinen zugriff", "kein zugriff",
];

/// Words that turn a marker into a denial. All matched on word boundaries:
/// an earlier substring version flagged "**lack**" inside "B**lack**Rock"
/// and "**cant**" inside "**cant**aloupe".
const DENIAL_VERBS: &[&str] = &[
    // Contractions arrive from normalize() split on the apostrophe, so
    // "can't" is " can t " and "doesn’t" is " doesn t ".
    "can t", "cant", "cannot", "can not",
    "don t", "dont", "do not", "doesn t", "doesnt", "does not",
    "didn t", "won t", "couldn t", "isn t",
    "unable", "no access", "lack", "lacks", "lacking",
    "habe keinen", "habe kein", "keinen zugriff", "nicht in der lage",
];

/// True when `text` looks like the model denying it has web access.
///
/// Requires both a marker and a denial verb, so a reply *about* searching
/// ("I searched for real-time prices and found…") is not flagged.
pub fn is_capability_denial(text: &str) -> bool {
    let t = normalize(text);
    any(&t, DENIAL_MARKERS) && any(&t, DENIAL_VERBS)
}

/// Nudge injected when an earlier reply in the same conversation denied
/// having web access. Measured 0/8 → 8/8 on the poisoned history that
/// prompted this module.
pub const CORRECTION_NOTE: &str = "CORRECTION: an assistant reply earlier in this conversation \
claimed to lack live web access, real-time data, or the ability to browse. That claim was FALSE — \
you have a working `web_search` tool and it is available right now. Disregard that reply as \
precedent: do not copy its refusal. When the question needs current information, search.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_questions_are_caught() {
        for q in [
            "What is the Bitcoin price rn?",
            "bitcoin price",
            "What's the price of Bitcoin right now?",
            "What does bitcoin cost right now?",   // definitional stem, still live
            "What does the euro cost today?",
            "Tell me the news of today.",
            "whats the weather tomorrow",
            "Who is the current chancellor of Germany?",
            "who is the president of france",
            "Was ist der Bitcoin Kurs gerade?",
            "Wie ist das Wetter heute?",
            "what happened today",
            "any news?",
            "what are the latest news",
            "AAPL stock price",
            "who won the game",
            "how much is bitcoin worth today",
        ] {
            assert!(needs_live_data(q), "should force a search: {q:?}");
        }
    }

    /// Every string here was found by an adversarial review of the first
    /// version, which matched raw substrings and fired on all of them.
    #[test]
    fn innocent_messages_do_not_force_a_search() {
        for q in [
            // "rate" inside ordinary verbs — the worst offender: every
            // image and story request forced a metered search.
            "Can you generate a picture of a dragon for Emma?",
            "generate a bedtime story about a fox",
            "Is that accurate?",
            "I'm so grateful for your help",
            "Please rate my essay from 1 to 10",
            "celebrate my birthday with a poem",
            // Bare nouns inside unrelated words or innocent senses.
            "What's the best time to visit Stockholm?",
            "Can I substitute chicken stock for broth in this soup?",
            "My son shares his toys with his sister, is that normal?",
            // German words whose common sense is not temporal.
            "Ist 7 eine gerade Zahl?",
            "Zeichne eine gerade Linie",
            "Ich habe einen Yoga Kurs gebucht",
            // Temporal word, nothing to look up.
            "How are you today?",
            "What should we cook for dinner tonight?",
            "I have a math test this week, can you quiz me",
            "good news! I passed my exam",
            // Definitional / timeless.
            "what is 2+2",
            "What is a MoE language model?",
            "what is bitcoin",
            "what is a stock",
            "explain how photosynthesis works",
            // Date/time is answered from the prompt and the datetime tool.
            "what time is it right now",
            "what's today's date?",
            // Meta.
            "what did you say today",
            "why did you search just now?",
            "can you summarize that",
            "Was hast du heute gesagt?",
            // Not a question.
            "thanks",
            "ok",
            "/deep",
        ] {
            assert!(!needs_live_data(q), "must NOT force a search: {q:?}");
        }
    }

    #[test]
    fn attachment_text_cannot_decide_to_search() {
        // The critical case: a user attaches a PDF and types something
        // innocuous. format_user concatenates the document after a blank
        // line, and the document mentions prices and dates constantly.
        let with_pdf = "summarise this for me\n\nInvoice 2026-07-29 — total cost \
            EUR 1,240.00. Current price list attached. Bitcoin accepted.";
        assert_eq!(typed_prose(with_pdf), "summarise this for me");
        assert!(
            !needs_live_data(with_pdf),
            "a private document must not trigger a web search"
        );
        // And the typed part still decides when IT is the live question.
        let live = "what is the bitcoin price today?\n\n[attached image: chart.png]";
        assert!(needs_live_data(live));
    }

    #[test]
    fn long_pasted_text_is_truncated_on_a_char_boundary() {
        let pasted = format!("{} bitcoin price", "ä".repeat(600));
        // Must not panic on a multibyte boundary, and the tail past the cap
        // is not classified.
        let _ = needs_live_data(&pasted);
        assert!(typed_prose(&pasted).chars().count() <= 400);
    }

    #[test]
    fn denials_are_recognised_including_the_ones_that_slipped_through() {
        for t in [
            "I don't have real-time market data access, so I can't give you the live Bitcoin price.",
            "I don't have access to live financial data or real-time price feeds.",
            "I don’t have live access to current events, and my training data only goes up to 2024.",
            "I don't have the ability to browse the web or access external links directly.",
            "I don't have live web access to verify the exact Ethernet speed.",
            "I can't provide a direct link, as my knowledge doesn't include live web browsing.",
            "I cannot browse the internet.",
            "Ich habe keinen Zugriff auf Echtzeitdaten.",
        ] {
            assert!(is_capability_denial(t), "should be flagged as a denial: {t:?}");
        }
    }

    #[test]
    fn talking_about_search_is_not_a_denial() {
        for t in [
            "I searched for the real-time Bitcoin price and found $63,204 (source: CoinGecko).",
            "The live data shows a 7.1 magnitude earthquake.",
            "Here are today's headlines, from a web search I just ran.",
            "I checked the internet access settings you asked about.",
            // Adversarial: denial VERBS hiding inside ordinary words. The
            // substring version flagged all of these.
            "BlackRock's real-time holdings are listed here.",
            "I found a cantaloupe recipe and the live data on melon prices.",
            "The candidate lacked real-time experience, per the article I found.",
        ] {
            assert!(!is_capability_denial(t), "false positive on: {t:?}");
        }
    }
}
