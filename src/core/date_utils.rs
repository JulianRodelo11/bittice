pub fn is_date_format(s: &str) -> bool {
    // Formato básico YYYY-MM-DD
    if s.len() >= 10 {
        let b = s.as_bytes();
        return b[4] == b'-' && b[7] == b'-';
    }
    false
}

pub fn has_time_component(s: &str) -> bool {
    // Busca T o : indicando hora
    s.contains('T') || s.contains(':')
}

pub fn extract_day(s: &str) -> Option<String> {
    if s.len() >= 10 {
        return Some(s[..10].to_string());
    }
    None
}

pub fn extract_month(s: &str) -> Option<String> {
    if s.len() >= 7 {
        return Some(s[..7].to_string());
    }
    None
}

pub fn extract_hour_bucket(s: &str) -> Option<String> {
    let time_part = s.split('T').nth(1).or_else(|| s.split(' ').nth(1))?;
    let hour = time_part.split(':').next()?;
    if let Ok(h) = hour.parse::<u8>() {
        return Some(format!("{:02}:00-{:02}:59", h, h));
    }
    None
}
