//! Dumps the REAL shipped system prompt so prompt-behaviour probes test the
//! actual wording instead of a paraphrase. #[ignore] — a tool, not a test.

#[test]
#[ignore = "developer tool: writes the current system prompt to /tmp"]
fn dump_system_prompt() {
    let msg = kinai::context::system_prompt("Our Family", "");
    let text = match msg {
        kinai::context::ChatMessage::System { content } => content,
        _ => panic!("expected a system message"),
    };
    std::fs::write("/tmp/kinai_system_prompt.txt", &text).unwrap();
    eprintln!("wrote {} chars", text.len());
}
