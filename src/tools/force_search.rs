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
    "latest news", "news today", "breaking news",
    "nachrichten heute", "headlines", "schlagzeilen",
    "who won", "wer hat gewonnen", "spielstand", "final score",
    // 2026-08-12 adversarial panel: "the news"/"any news" moved OUT of
    // STRONG — "Any news from Oma?" and "I told my husband the news that
    // I'm pregnant" are personal news and were force-searching. A bare
    // "any news?" still forces via the whole-message check in
    // needs_live_data. Added below: confirmed misses from the same panel.
    "was gibt es neues", "was gibt s neues", "neuesten nachrichten",
    "flight on time", "flug pünktlich", "flug puenktlich",
    "temperature outside", "weather outside", "wie warm ist es", "wie kalt ist es",
    "won last night", "win last night", "game last night",
];

/// Live subjects that need a partner signal, because each has a common
/// innocent sense ("chicken stock", "Yoga Kurs", "good news!").
const TOPIC: &[&str] = &[
    "news", "weather", "wetter", "temperature", "forecast",
    "score", "scores", "standings", "ergebnis", "ergebnisse", "traffic",
    "stock", "stocks", "aktie", "aktien", "kurs",
    "bitcoin", "btc", "ethereum", "crypto", "dollar", "euro",
    // Travel (2026-08-12): "best flights this week to Belize" went
    // unforced because nothing travel-shaped was listed. Singular AND
    // plural — matching is word-boundary, one does not imply the other.
    "flight", "flights", "airfare", "airfares", "airline", "airlines",
    "hotel", "hotels", "flug", "flüge", "fluege", "flugpreis", "flugpreise",
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
    "momentan", "neueste", "neuesten", "gestern", "morgen",
    // Panel 2026-08-12: "What's the weather on Friday?" had a live topic
    // and a day reference yet no listed temporal word. Weekdays are
    // deictic in chat — nobody asks about a *past* Friday's weather.
    "monday", "tuesday", "wednesday", "thursday", "friday", "saturday",
    "sunday", "montag", "dienstag", "mittwoch", "donnerstag", "freitag",
    "samstag", "sonntag", "this evening", "this afternoon", "last night",
    "am wochenende", "letzte nacht", "gestern abend",
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
    // "you say" and bare "what did i" were too greedy: they vetoed
    // "What would you say the bitcoin price is today" and "What did I
    // miss in the news today" (panel 2026-08-12). Narrowed to the
    // conversation-referencing phrasings.
    "you said", "you just", "your last", "your previous", "your answer",
    "your reply", "did you search", "why did you", "what can you do",
    "how do you work", "who are you", "what are you", "your name",
    "summarize that", "summarise that", "explain that", "rephrase",
    "translate that", "what did you", "what did i say", "what did i ask",
    "what did i tell", "what do you mean",
    "repeat that", "say that again", "was hast du", "was habe ich", "wer bist du",
];

/// The date and time are already injected into every system prompt, and a
/// `datetime` tool exists for them. Forcing a *web* search for "what time
/// is it" would be slower and worse, and the forced round offers only
/// `web_search`, so it would also hide the right tool.
const DATETIME_ONLY: &[&str] = &[
    // Panel 2026-08-12: bare "what time"/"what day" swallowed "what time
    // does the game start today" and "what time does the DMV open" —
    // live questions. Only the is-it forms are pure clock/date questions.
    "what time is it", "what time it is", "whats the time",
    "what day is it", "what day it is", "whats the date", "what is the date",
    "todays date", "what year is it", "wie viel uhr", "wie spät ist es",
    "wie spaet ist es", "welcher tag ist", "welches datum",
    // normalize() splits apostrophes into spaces, so "what's today's date?"
    // becomes "what s today s date" — none of the entries above match it.
    // Latent since day one; exposed when the temporal-alone rule made bare
    // "today" decisive. Cover the apostrophe-split shapes explicitly.
    "what s the time", "what s the date", "today s date", "what s today s date",
];

/// Deictic present/future phrases that force a search ON THEIR OWN.
///
/// Owner rule (Wolf, 2026-08-12): "Everything with today, this week, or
/// dates in the future should be searched online." The two-signal
/// discipline above stays for fuzzy cases, but these words anchor the
/// question to *now or later* — the one region of time the model's
/// weights cannot know — so a second signal is no longer required.
///
/// Deliberately absent: "now" and "rn" (overwhelmingly conversational
/// filler — "I get it now", "do it now"), "yesterday"/"gestern" (past;
/// still forces via the two-signal paths when paired with a topic), and
/// bare "current" (adjective glue: "my current setup"). Those keep
/// needing a second signal.
const TEMPORAL_ALONE: &[&str] = &[
    "today", "todays", "tonight", "tomorrow", "this week", "this weekend",
    "this month", "this year", "next week", "next weekend", "next month",
    "next year", "right now", "at the moment", "as of now", "currently",
    "latest", "most recent",
    "heute", "heute abend", "morgen", "übermorgen", "uebermorgen",
    "diese woche", "dieses wochenende", "diesen monat", "dieses jahr",
    "nächste woche", "naechste woche", "nächsten monat", "naechsten monat",
    "nächstes jahr", "naechstes jahr", "aktuell", "aktuelle", "aktuellen",
    "derzeit", "momentan", "zurzeit", "neueste", "neuesten", "kürzlich",
    "kuerzlich", "this evening", "this afternoon", "am wochenende",
    "monday", "tuesday", "wednesday", "thursday", "friday", "saturday",
    "sunday", "montag", "dienstag", "mittwoch", "donnerstag", "freitag",
    "samstag", "sonntag",
];

/// Creative-generation asks. Checked GLOBALLY, before even STRONG: the
/// 2026-08-12 panel found "Tell me a bedtime story about who won the
/// dragon race" force-searching via STRONG's "who won". Accepted cost:
/// "write a summary of today's news" is no longer forced either — a
/// creative framing wins over everything, which is the module's oldest
/// lesson (v1's worst fires were all creative requests).
const CREATIVE_VETO: &[&str] = &[
    "quiz me", "test me on", "frag mich ab", "abfragen",
    "write me", "write a", "write us", "tell me a story", "tell us a story",
    "tell her a story", "tell him a story", "bedtime story", "gutenachtgeschichte",
    "a story", "eine geschichte", "story for", "geschichte über", "geschichte ueber",
    "erzähl", "erzaehl", "generate a", "generate an", "draw me", "draw a",
    "draw us", "mal mir", "mal uns", "make up", "invent a", "schreib mir",
    "schreibe", "zeichne", "erfinde", "gedicht", "poem", "a joke", "a riddle",
    "a song", "ein witz", "einen witz", "ein rätsel", "ein raetsel", "ein lied",
    "sing us", "sing me", "sing mir",
];

/// Greetings, farewells, thanks, wellbeing — smalltalk where a temporal
/// word is social glue ("see you TOMORROW", "guten MORGEN"). Consulted by
/// the temporal-alone rule only, so "guten morgen, wie wird das wetter
/// heute?" still forces via topic+temporal before this list is reached.
const SMALLTALK_VETO: &[&str] = &[
    "how are you", "how r u", "how are u", "how was your", "how s your",
    "how is your", "hows your", "how s it going", "how is it going",
    "hows it going", "how s everyone", "how did you sleep", "wie geht",
    "wie war dein", "wie war euer", "wie hast du geschlafen",
    "guten morgen", "guten abend", "gute nacht", "good morning",
    "good night", "goodnight", "see you", "bis morgen", "bis später",
    "bis spaeter", "bis nachher", "bis gleich", "bis nächste", "bis naechste",
    "schlaf gut", "sleep well", "thanks", "thank you", "danke",
    "was kannst du",
];

/// Requests aimed at KinAI's own task tools (reminders, lists, calendar)
/// or at schoolwork. Forcing a round where web_search is the ONLY tool
/// would actively hide the right tool from the model — worse than not
/// forcing (the DATETIME_ONLY lesson, generalised; panel 2026-08-12).
const TASK_VETO: &[&str] = &[
    "remind", "reminder", "erinnere", "erinnerung", "calendar", "kalender",
    "termin", "appointment", "shopping list", "einkaufsliste", "todo",
    "to do", "alarm", "timer", "hausaufgaben", "vokabeln", "homework",
];

/// The owner rule only fires on messages shaped like an information
/// request. Statements never force: "See you tomorrow!", "Thanks for
/// your help today!", "Not right now, thanks", "I had a bad day today,
/// can we talk?" all lack every stem below (panel 2026-08-12 — each of
/// those was a confirmed junk fire when temporal-alone decided by
/// itself). Broad single words like "is"/"was"/"wie" are safe HERE
/// because this list is only consulted after a temporal/year signal
/// already matched.
const REQUEST_SHAPE: &[&str] = &[
    "what", "whats", "which", "where", "when", "who", "whos", "why is",
    "how much", "how many", "how long", "how late", "how warm", "how cold",
    "how expensive", "is", "are", "does", "do", "can you find",
    "can you check", "can you search", "can you look", "find", "search",
    "look up", "check", "show me", "list", "best", "top", "cheapest",
    "any", "recommend", "compare", "available", "open",
    "was", "wo", "wann", "wer", "wie viel", "wieviel", "wie lange",
    "wie teuer", "welche", "welcher", "welches", "gibt es", "gibts",
    "ist", "sind", "hat", "haben", "kann man", "such", "suche", "finde",
    "zeig mir", "empfiehl", "günstigste", "guenstigste", "beste", "besten",
];

/// Live-by-construction phrases that need no temporal word at all
/// (panel false-misses): release dates and opening status are always
/// about now or the future.
const RELEASE_STEM: &[&str] = &[
    "come out", "comes out", "coming out", "release date", "releases",
    "kommt raus", "erscheint", "erscheinungsdatum", "wann kommt",
];
const OPEN_NOW: &[&str] = &[
    "still open", "open now", "open today", "open tonight", "open right now",
    "noch offen", "noch auf", "noch geöffnet", "noch geoeffnet", "offen heute",
];

/// "Who is the king in The Lion King?" — a role question about fiction
/// (panel 2026-08-12). Applied to the WHO+ROLE path only.
const FICTION_VETO: &[&str] = &[
    "in the movie", "in the film", "in the book", "in the story",
    "in the show", "in the series", "im film", "im buch", "in der serie",
    "in der geschichte", "im märchen", "im maerchen",
];

/// Arithmetic context around a number: "Is 2027 a prime number?" and
/// "Was ist 2026 mal 4?" are homework, not calendar references
/// (panel 2026-08-12). Consulted by the year rule only, so common words
/// like German "mal" cannot suppress anything else.
const MATH_CONTEXT: &[&str] = &[
    "prime", "primzahl", "divided", "divide", "geteilt", "teilbar",
    "multiply", "multiplied", "times", "plus", "minus", "squared",
    "square root", "wurzel", "quersumme", "mal", "even number", "odd number",
];

/// A year token at or after the current year ("world cup 2026",
/// "olympics 2028", "die 2026er Steuerreform") anchors the question to
/// the present or future exactly like a deictic word does.
///
/// Guardrails, each from a confirmed panel finding:
///  * window is current year .. +10 — "RTX 2070" and "Cyberpunk 2077"
///    are products, not plans for the 2070s;
///  * two or more standalone numbers = arithmetic ("Was ist 2026 mal 4?"),
///    as is any MATH_CONTEXT word ("Is 2027 a prime number?");
///  * past years stay historical ("population of Rome in 1950");
///  * "2026er" (German adjectival year) counts.
fn has_current_or_future_year(norm: &str) -> bool {
    let this_year = chrono::Local::now().format("%Y").to_string();
    let Ok(this_year) = this_year.parse::<u32>() else {
        return false;
    };
    if any(norm, MATH_CONTEXT) {
        return false;
    }
    let number_tokens = norm
        .split_whitespace()
        .filter(|t| t.chars().all(|c| c.is_ascii_digit()))
        .count();
    if number_tokens >= 2 {
        return false;
    }
    let year_of = |tok: &str| -> Option<u32> {
        let digits = if tok.len() == 4 && tok.chars().all(|c| c.is_ascii_digit()) {
            tok
        } else if tok.len() == 6 && tok.ends_with("er") {
            &tok[..4]
        } else {
            return None;
        };
        digits.parse::<u32>().ok()
    };
    norm.split_whitespace().any(|tok| {
        year_of(tok).is_some_and(|y| y >= this_year && y <= this_year + 10)
    })
}

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
    // Creative framing wins over everything, including STRONG — the
    // panel's "bedtime story about who won the dragon race" fired via
    // "who won" until this moved ahead of it.
    if any(&q, CREATIVE_VETO) {
        return false;
    }
    // Conversation-meta and pure clock/date questions never search.
    if any(&q, META) || any(&q, DATETIME_ONLY) {
        return false;
    }
    if any(&q, STRONG) || (any(&q, WHO_STEM) && any(&q, ROLE_NOUN) && !any(&q, FICTION_VETO)) {
        return true;
    }
    // A bare "news?" / "any news?" with nothing else IS a headline
    // request — the personal-news problem ("any news from Oma?") only
    // exists once more words follow.
    if matches!(q.trim(), "news" | "any news" | "the news" | "nachrichten") {
        return true;
    }
    // Release dates and opening status are about now/future by
    // construction, no temporal word needed.
    if any(&q, RELEASE_STEM) || any(&q, OPEN_NOW) {
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
    if (value && topic)
        || (topic && temporal)
        || (value && temporal)
        || (any(&q, EVENT_STEM) && temporal)
    {
        return true;
    }
    // Owner rule (Wolf, 2026-08-12), after "best flights this week to
    // Belize" fell through the two-signal net: a deictic present/future
    // phrase — or a current/future year — forces a search WITHOUT a
    // second subject signal. Three containment walls, each one a class
    // of confirmed junk fire from the 2026-08-12 adversarial panel:
    //   REQUEST_SHAPE   statements never force ("See you tomorrow!")
    //   SMALLTALK_VETO  greetings/thanks with a question shape
    //                   ("How was your day today?", "Guten Morgen, wie
    //                   hast du geschlafen?")
    //   TASK_VETO       reminder/calendar/homework asks, where forcing
    //                   web_search-only would hide the right tool
    // The year path skips the shape gate: "olympics 2028 host city" is a
    // terse noun-phrase query with no stem, and the year fn already
    // filters math/product numbers. Statements with future years ("See
    // you in 2027!") are caught by the smalltalk veto or accepted as the
    // rare over-fire the owner prefers to a miss.
    let anchored = (any(&q, TEMPORAL_ALONE) && any(&q, REQUEST_SHAPE))
        || has_current_or_future_year(&q);
    anchored && !any(&q, SMALLTALK_VETO) && !any(&q, TASK_VETO)
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

    /// Owner rule (2026-08-12): a deictic present/future phrase alone, or a
    /// current/future year, forces a search — on information requests.
    /// Includes every confirmed false-MISS from the 2026-08-12 adversarial
    /// panel (36 agents, 32 confirmed findings).
    #[test]
    fn temporal_alone_and_future_years_force_a_search() {
        for q in [
            // The two field failures that motivated the rule.
            "Can you find the best flights this week to Belize?",
            "Who is the winner of the soccer world cup 2026?",
            // Travel topics count for the two-signal paths now.
            "how much are flights to Belize",
            "best hotel prices in San Pedro",
            // Deictic-alone information requests.
            "what movies are playing this weekend",
            "are there any events in Miami tomorrow",
            "what should I do this weekend in Miami",
            "What should we cook for dinner tonight?", // owner-accepted flip
            // Current/future years (window: this year .. +10).
            "olympics 2028 host city",
            "Wer hat die WM 2026 gewonnen?",
            "Steuerfristen 2026",
            "Was ändert sich mit der 2026er Steuerreform?", // adjectival year
            // German deictic-alone.
            "Welche Filme laufen diese Woche?",
            "Was ist morgen in Miami los?",
            "Was ist am Wochenende in Miami los?",
            // Panel false-misses, now covered.
            "What time does the game start today?",   // DATETIME_ONLY was too greedy
            "What time does the DMV open today?",
            "What's the weather on Friday?",           // weekdays are temporal
            "When does the new iPhone come out?",      // release stem
            "Is the store still open?",                // open-now stem
            "What's the temperature outside?",
            "Did the Dolphins win last night?",
            "Is our flight on time?",
            "Was sind die neuesten Nachrichten?",
            "Was gibt's Neues?",
            "What did I miss in the news today?",      // META was too greedy
            "What would you say the bitcoin price is today?",
        ] {
            assert!(needs_live_data(q), "should force a search: {q:?}");
        }
    }

    /// Every confirmed false-FIRE from the 2026-08-12 adversarial panel.
    /// Statements, smalltalk, creative asks, task-tool requests, math on
    /// year-sized numbers, and product numbers must never burn a search.
    #[test]
    fn panel_false_fires_stay_silent() {
        for q in [
            // Statements and deferrals -- no REQUEST_SHAPE stem.
            "Not right now, thanks",
            "See you tomorrow!",
            "Thanks for your help today!",
            "I had a bad day today, can we talk?",
            "I recently started piano lessons, any tips?",
            // Greeting class, including question-shaped variants.
            "How are you today?",
            "How was your day today?",
            "Wie war dein Tag heute?",
            "Was kannst du heute für mich tun?",
            "Guten Morgen!",
            "Guten Morgen, wie hast du geschlafen?",
            "Bis morgen!",
            "Danke, bis morgen",
            // Creative asks, third-person included.
            "Can you tell Emma a story tonight?",
            "tell me a bedtime story tonight",
            "write a poem about today",
            "generate a picture of a sunset tomorrow evening",
            "Schreibe ein Gedicht über heute",
            "Erzähl mir eine Gutenachtgeschichte für heute Abend",
            "Mal mir ein Bild von einem Einhorn für morgen",
            // Quiz / homework, both languages.
            "I have a math test this week, can you quiz me",
            "Ich habe morgen einen Mathetest, kannst du mich abfragen?",
            "Kannst du mir bei den Hausaufgaben für heute helfen?",
            // Task-tool requests -- forcing web_search would hide the right tool.
            "Can you remind me to call grandma tomorrow?",
            "Ist die Einkaufsliste noch aktuell?",
            "Ist der Termin morgen noch aktuell?",
            // Year-sized numbers that are not years.
            "Is 2027 a prime number?",
            "Was ist 2026 mal 4?",
            "Was ist 2048 mal 2?",
            "Is the RTX 2070 good enough for Fortnite?",
            "how do I beat the final boss in Cyberpunk 2077",
            // Personal news is not headlines.
            "Any news from Oma?",
            "I told my husband the news that I'm pregnant!",
            // Past years stay historical.
            "what was the population of Rome in 1950",
            "the treaty was signed in 1998",
        ] {
            assert!(!needs_live_data(q), "must NOT force a search: {q:?}");
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
            // Temporal word in smalltalk / quiz framing.
            // NOTE: "What should we cook for dinner tonight?" used to sit
            // here and now deliberately DOES force — owner rule 2026-08-12
            // says deictic-now questions get live results.
            "How are you today?",
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
