pub fn random_hex(bytes: usize) -> String {
    let mut buf = Vec::with_capacity(bytes);
    while buf.len() < bytes {
        let chunk = uuid::Uuid::new_v4();
        let take = (bytes - buf.len()).min(16);
        buf.extend_from_slice(&chunk.as_bytes()[..take]);
    }
    hex::encode(buf)
}

/// Rough token estimate, good enough for usage reporting.
pub fn approx_tokens(s: &str) -> u64 {
    ((s.chars().count() as f64) / 3.5).ceil() as u64
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}...")
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
