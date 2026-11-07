//! Framework and infrastructure classification from process arguments.

use super::matcher::{CheckoutIndex, usable_ancestors};
use super::procinfo::ProcInfo;
use super::redact::redact;
use lantern_core::model::ListenerClass;
use std::path::Path;

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}
fn rule(args: &[String]) -> Option<(ListenerClass, String)> {
    let words: Vec<String> = args
        .iter()
        .map(|s| {
            Path::new(s)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(s)
                .to_ascii_lowercase()
        })
        .collect();
    let has = |s: &str| words.iter().any(|w| w == s);
    let any = |values: &[&str]| values.iter().any(|s| has(s));
    let dev = ListenerClass::DevServer;
    let rules: &[(&[&str], &[&str], &str)] = &[
        (&["vite"], &[], "Vite"),
        (&["next"], &["dev", "start"], "Next.js"),
        (&["nuxt", "nuxi"], &["dev"], "Nuxt"),
        (&["astro"], &["dev"], "Astro"),
        (&["remix"], &[], "Remix"),
        (&["svelte-kit"], &[], "SvelteKit"),
        (&["ng"], &["serve"], "Angular"),
        (&["webpack"], &["serve"], "Webpack"),
        (&["webpack-dev-server"], &[], "Webpack"),
        (&["parcel"], &[], "Parcel"),
        (&["storybook"], &[], "Storybook"),
        (&["expo"], &["start"], "Expo"),
        (&["wrangler"], &["dev"], "Wrangler"),
        (&["gatsby"], &["develop"], "Gatsby"),
        (&["vitepress"], &["dev"], "VitePress"),
        (&["docusaurus"], &["start"], "Docusaurus"),
        (&["react-scripts"], &["start"], "Create React App"),
        (&["uvicorn"], &[], "uvicorn"),
        (&["gunicorn"], &[], "gunicorn"),
        (&["hypercorn"], &[], "hypercorn"),
        (&["daphne"], &[], "Daphne"),
        (&["granian"], &[], "Granian"),
        (&["manage"], &["runserver"], "Django"),
        (&["flask"], &["run", "-m"], "Flask"),
        (&["fastapi"], &["dev", "run"], "FastAPI"),
        (&["streamlit"], &["run"], "Streamlit"),
        (&["mkdocs"], &["serve"], "MkDocs"),
        (&["http"], &["-m"], "Python HTTP server"),
        (&["artisan"], &["serve"], "Laravel serve"),
        (&["php"], &["-s"], "PHP built-in"),
        (&["symfony"], &["serve"], "Symfony"),
        (&["rails"], &["s", "server"], "Rails"),
        (&["puma"], &[], "Puma"),
        (&["rackup"], &[], "Rackup"),
        (&["jekyll"], &["serve"], "Jekyll"),
        (&["hugo"], &["server"], "Hugo"),
        (
            &["serve", "http-server", "live-server", "browser-sync"],
            &[],
            "Static server",
        ),
        (&["caddy"], &["file-server"], "Static server"),
        (&["air"], &[], "air"),
        (&["cargo", "go"], &["run"], "Development server"),
    ];
    if args
        .first()
        .is_some_and(|a| basename(a).starts_with("next-server"))
    {
        return Some((dev, "Next.js".into()));
    }
    for (signals, subcommands, label) in rules {
        if any(signals) && (subcommands.is_empty() || any(subcommands)) {
            return Some((dev, (*label).into()));
        }
    }
    for name in [
        "postgres",
        "redis-server",
        "mysqld",
        "mariadbd",
        "mongod",
        "memcached",
        "nginx",
        "apache2",
        "php-fpm",
        "caddy",
        "dnsmasq",
        "sshd",
        "cupsd",
        "dockerd",
        "containerd",
        "mailpit",
        "minio",
    ] {
        if words
            .iter()
            .any(|w| w == name || (name == "php-fpm" && w.starts_with("php-fpm:")))
        {
            return Some((ListenerClass::Infra, name.into()));
        }
    }
    None
}
fn fallback(p: Option<&ProcInfo>) -> String {
    let Some(p) = p else {
        return "Unknown process".into();
    };
    let safe_args = redact(&p.cmdline_raw);
    let exe = p
        .exe
        .as_deref()
        .and_then(Path::file_name)
        .and_then(|s| s.to_str())
        .or_else(|| safe_args.first().map(|s| basename(s)))
        .unwrap_or("process");
    let script = if [
        "node", "bun", "deno", "tsx", "nodemon", "python", "python3", "ruby", "php",
    ]
    .contains(&exe)
    {
        safe_args
            .iter()
            .skip(1)
            .find(|s| !s.starts_with('-'))
            .map(|s| basename(s))
    } else {
        None
    };
    let raw = script.map_or_else(|| exe.to_owned(), |s| format!("{exe} {s}"));
    raw.chars().filter(|c| !c.is_control()).take(120).collect()
}
pub(super) fn classify(
    p: Option<&ProcInfo>,
    uid: u32,
    me: u32,
    index: &CheckoutIndex,
    matched: bool,
    daemon_pid: u32,
    docker: bool,
) -> (ListenerClass, String) {
    if docker {
        return (ListenerClass::Docker, "Docker".into());
    }
    let Some(proc) = p else {
        return (
            if uid == me {
                ListenerClass::Other
            } else {
                ListenerClass::Foreign
            },
            "Unknown process".into(),
        );
    };
    let exe = proc
        .exe
        .as_deref()
        .and_then(Path::file_name)
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if ["rootlesskit", "rootlessport"].contains(&exe) {
        return (ListenerClass::Docker, "Docker (rootless)".into());
    }
    if let Some(unit) = proc.unit.as_deref().and_then(|u| u.strip_prefix("lantern-svc-")) {
        return (
            ListenerClass::Infra,
            unit.trim_end_matches(".service")
                .trim_end_matches(".scope")
                .to_owned(),
        );
    }
    let rules: Vec<_> = std::iter::once(proc)
        .chain(usable_ancestors(proc, me, daemon_pid))
        .filter_map(|p| rule(&p.cmdline_raw))
        .collect();
    let label = rules
        .iter()
        .find(|(c, _)| *c == ListenerClass::DevServer)
        .map(|(_, l)| l.clone())
        .unwrap_or_else(|| fallback(Some(proc)));
    if matched {
        return (ListenerClass::Project, label);
    }
    if let Some((_, label)) = rules.iter().find(|(c, _)| *c == ListenerClass::DevServer) {
        return (ListenerClass::DevServer, label.clone());
    }
    if proc.cwd.as_ref().is_some_and(|cwd| {
        index
            .workspace_roots
            .iter()
            .any(|root| cwd != root && cwd.starts_with(root))
    }) {
        return (ListenerClass::DevServer, label);
    }
    if let Some((_, label)) = rules.iter().find(|(c, _)| *c == ListenerClass::Infra) {
        return (ListenerClass::Infra, label.clone());
    }
    (
        if uid != me {
            ListenerClass::Foreign
        } else {
            ListenerClass::Other
        },
        label,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_classify_rules() {
        for (args, expected) in [
            (
                &["node", "./node_modules/.bin/vite", "--port", "5173"][..],
                "Vite",
            ),
            (&["next", "dev"], "Next.js"),
            (&["python", "manage.py", "runserver"], "Django"),
            (&["python", "-m", "http.server"], "Python HTTP server"),
            (&["php", "artisan", "serve"], "Laravel serve"),
            (&["rails", "server"], "Rails"),
            (&["redis-server"], "redis-server"),
        ] {
            let args: Vec<_> = args.iter().map(|s| s.to_string()).collect();
            assert_eq!(rule(&args).unwrap().1, expected);
        }
    }
}
