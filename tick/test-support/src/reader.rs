use std::sync::{Arc, RwLock};

use tick_market::reader::{Document, Library};

pub fn mock_library() -> Library {
    let mut document = Document::from_text(
        "demo://earnings/FY2026Q2",
        "DEMO · FY2026 Q2 Earnings Release",
        "# Quarterly results\n\nDEMO: fictional company and financial figures.\n\n## Revenue\n\nRevenue was $12.8 billion. Growth came from enterprise services.\n\n| Segment | Revenue |\n| --- | --- |\n| Enterprise | 8.4 billion |\n| Consumer | 4.4 billion |\n\n## Cash flow\n\nOperating cash flow was $3.4 billion; capital expenditures were $1.1 billion.\n\n## Outlook\n\nManagement expects investment spending to remain elevated. These examples are not live disclosures.",
    );
    document.symbol = Some("DEMO.US".into());
    document.fiscal_period = Some("FY2026Q2".into());
    document
        .warnings
        .push("Simulated document; no company disclosure was fetched".into());
    Arc::new(RwLock::new(vec![document]))
}
