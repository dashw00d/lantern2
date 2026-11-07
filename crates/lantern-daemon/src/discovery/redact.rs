//! Secret removal before command lines leave discovery.

fn secret_key(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    [
        "token",
        "secret",
        "passwd",
        "password",
        "pwd",
        "apikey",
        "api-key",
        "api_key",
        "auth",
        "credential",
        "bearer",
        "cookie",
        "session",
        "private",
        "dsn",
    ]
    .iter()
    .any(|key| lower.contains(key))
}
fn token(s: &str) -> bool {
    if s.len() < 32
        || s.starts_with(['/', '.', '~'])
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.+/=".contains(&b))
    {
        return false;
    }
    let kinds = [
        s.bytes().any(|b| b.is_ascii_lowercase()),
        s.bytes().any(|b| b.is_ascii_uppercase()),
        s.bytes().any(|b| b.is_ascii_digit()),
    ];
    kinds.into_iter().filter(|x| *x).count() >= 2
}
fn urls(s: &str) -> String {
    let mut result = String::new();
    let mut rest = s;
    while let Some(pos) = rest.find("://") {
        let prefix_end = pos + 3;
        result.push_str(&rest[..prefix_end]);
        rest = &rest[prefix_end..];
        let end = rest.find(['/', '?', '#', ' ', '\t', '\n']).unwrap_or(rest.len());
        let authority = &rest[..end];
        if let Some((info, host)) = authority.rsplit_once('@') {
            if let Some((user, _)) = info.split_once(':') {
                result.push_str(user);
                result.push_str(":***@");
            } else {
                result.push_str("***@");
            }
            result.push_str(host);
        } else {
            result.push_str(authority);
        }
        rest = &rest[end..];
    }
    result.push_str(rest);
    result
}
fn shorten(s: &str) -> String {
    let mut chars = s.chars();
    let result: String = chars.by_ref().take(200).collect();
    if chars.next().is_some() {
        format!("{result}…")
    } else {
        result
    }
}
/// Return only redacted, bounded arguments.
pub(super) fn redact(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len().min(41));
    let mut next_secret = false;
    for arg in args.iter().take(40) {
        let replacement = if next_secret {
            next_secret = false;
            "***".to_owned()
        } else if let Some((key, _)) = arg.split_once('=')
            && secret_key(key)
        {
            format!("{key}=***")
        } else if arg.starts_with('-') && !arg.contains('=') && (secret_key(arg) || arg == "-p") {
            next_secret = true;
            arg.clone()
        } else if token(arg) {
            "***".to_owned()
        } else {
            urls(arg)
        };
        out.push(shorten(&replacement));
    }
    if args.len() > 40 {
        out.push("…".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_redact_secret_corpus() {
        let keys = [
            "token",
            "SECRET",
            "password",
            "passwd",
            "pwd",
            "api_key",
            "API-KEY",
            "apikey",
            "authorization",
            "credential",
            "bearer",
            "cookie",
            "session",
            "private",
            "dsn",
        ];
        for key in keys {
            let args = vec![
                format!("--{key}=super-secret"),
                format!("{key}=super-secret"),
                format!("--{key}"),
                "super-secret".into(),
            ];
            let out = redact(&args);
            assert_eq!(out[0], format!("--{key}=***"));
            assert_eq!(out[1], format!("{key}=***"));
            assert_eq!(out[3], "***");
        }
        let args: Vec<String> = [
            "https://alice:hunter2@example.test/a",
            "postgres://abc123@example.test/db",
            "--db=postgres://alice:pw@db/test",
            "-p",
            "hunter2",
            "abcdefghijklmnopqrstuvwxyzABCDEF123",
            "node",
            "./node_modules/.bin/vite",
            "--port",
            "5173",
            "/a/very/long/path/abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        let out = redact(&args);
        assert_eq!(out[0], "https://alice:***@example.test/a");
        assert_eq!(out[1], "postgres://***@example.test/db");
        assert_eq!(out[2], "--db=postgres://alice:***@db/test");
        assert_eq!(out[4], "***");
        assert_eq!(out[5], "***");
        assert_eq!(out[6..], args[6..]);
        let many = vec!["a".repeat(210); 41];
        let out = redact(&many);
        assert_eq!(out.len(), 41);
        assert_eq!(out[0].chars().count(), 201);
        assert_eq!(out[40], "…");
    }
}
