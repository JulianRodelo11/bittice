pub fn is_date_format(s: &str) -> bool {
    // Formato básico YYYY-MM-DD
    if s.len() >= 10 {
        let b = s.as_bytes();
        // Verificamos que b[4] y b[7] sean '-'
        if b[4] != b'-' || b[7] != b'-' {
            return false;
        }
        // Verificamos que el resto sean dígitos (YYYY-MM-DD)
        let is_digit = |idx: usize| b[idx].is_ascii_digit();
        let ok = is_digit(0) && is_digit(1) && is_digit(2) && is_digit(3) // YYYY
            && is_digit(5) && is_digit(6)                               // MM
            && is_digit(8) && is_digit(9);                              // DD
            
        if !ok { return false; }

        // Validaciones numéricas básicas para evitar falsos positivos (como IDs con guiones)
        if let (Ok(month), Ok(day)) = (s[5..7].parse::<u8>(), s[8..10].parse::<u8>()) {
            return (1..=12).contains(&month) && (1..=31).contains(&day);
        }
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
