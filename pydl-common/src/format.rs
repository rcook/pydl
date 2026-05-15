#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn humanize_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let kib = bytes as f64 / 1024.0;
    if kib < 1024.0 {
        return format!("{kib:.1} KiB");
    }
    let mib = kib / 1024.0;
    if mib < 1024.0 {
        return format!("{mib:.1} MiB");
    }
    let gib = mib / 1024.0;
    format!("{gib:.1} GiB")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_bytes_small() {
        assert_eq!(humanize_bytes(0), "0 B");
        assert_eq!(humanize_bytes(512), "512 B");
        assert_eq!(humanize_bytes(1023), "1023 B");
    }

    #[test]
    fn humanize_bytes_kib() {
        assert_eq!(humanize_bytes(1024), "1.0 KiB");
        assert_eq!(humanize_bytes(1536), "1.5 KiB");
    }

    #[test]
    fn humanize_bytes_mib() {
        assert_eq!(humanize_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(humanize_bytes(26_843_546), "25.6 MiB");
    }

    #[test]
    fn humanize_bytes_gib() {
        assert_eq!(humanize_bytes(1024 * 1024 * 1024), "1.0 GiB");
    }
}
