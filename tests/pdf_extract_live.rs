//! Exercises the real PDF attachment path after the pdf-extract 0.7 -> 0.12
//! upgrade (which moves lopdf onto 0.42, fixing RUSTSEC-2026-0187).
//! #[ignore] — needs KINAI_PDF=<path to a real pdf>.
use base64::Engine as _;
use kinai::db::Attachment;

#[test]
#[ignore = "live: needs KINAI_PDF=<path to a real pdf>"]
fn a_real_pdf_attachment_still_yields_its_text() {
    let path = std::env::var("KINAI_PDF").expect("KINAI_PDF=<path to a pdf>");
    let bytes = std::fs::read(&path).expect("read the pdf");
    let att = Attachment {
        kind: "file".into(),
        mime: Some("application/pdf".into()),
        name: Some("notice.pdf".into()),
        data_url: Some(format!(
            "data:application/pdf;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        )),
    };
    let text = kinai::attachments::extract_text(&[att]).expect("extraction must succeed");
    println!("\n--- extracted ---\n{text}\n--- end ---\n");
    assert!(
        text.to_lowercase().contains("quick brown fox"),
        "the PDF's own words must come back: {text}"
    );
}
