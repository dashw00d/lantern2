use clap::Parser;
use lantern_cli::{
    CliAction, UnassembledSystem,
    args::{Cli, McpScope, McpTarget},
    dispatch,
    mcp_install::{install_claude_with_program, install_file},
};
use std::{fs, os::unix::fs::PermissionsExt};

#[test]
fn mcp_install_temp_files_preserve_other_entries_comments_mode_and_backup() {
    let temp = tempfile::tempdir().unwrap();
    let codex = temp.path().join("codex/config.toml");
    fs::create_dir_all(codex.parent().unwrap()).unwrap();
    let original = "# personal settings\n[mcp_servers.other]\nurl = 'https://example.test'\n\n[mcp_servers.lantern]\nurl = 'http://127.0.0.1:39600/mcp' # keep this\n";
    fs::write(&codex, original).unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o640)).unwrap();
    let url = "http://127.0.0.1:39601/mcp";
    assert!(install_file(&codex, McpTarget::Codex, url, false).is_err());
    assert_eq!(fs::read_to_string(&codex).unwrap(), original);
    assert!(install_file(&codex, McpTarget::Codex, url, true).unwrap());
    let output = fs::read_to_string(&codex).unwrap();
    assert!(
        output.contains("# personal settings")
            && output.contains("# keep this")
            && output.contains("https://example.test")
    );
    assert!(output.contains("tool_timeout_sec = 120") && output.contains(url));
    assert_eq!(
        fs::read_to_string(temp.path().join("codex/config.toml.lantern-bak")).unwrap(),
        original
    );
    assert_eq!(fs::metadata(&codex).unwrap().permissions().mode() & 0o777, 0o640);
    assert!(!install_file(&codex, McpTarget::Codex, url, false).unwrap());

    let cursor = temp.path().join("cursor/mcp.json");
    fs::create_dir_all(cursor.parent().unwrap()).unwrap();
    fs::write(
        &cursor,
        r#"{"theme":"dark","mcpServers":{"other":{"url":"https://example.test"}}}"#,
    )
    .unwrap();
    assert!(install_file(&cursor, McpTarget::Cursor, url, false).unwrap());
    let value: serde_json::Value = serde_json::from_slice(&fs::read(&cursor).unwrap()).unwrap();
    assert_eq!(value["theme"], "dark");
    assert_eq!(value["mcpServers"]["other"]["url"], "https://example.test");
    assert_eq!(value["mcpServers"]["lantern"]["url"], url);
    assert!(!install_file(&cursor, McpTarget::Cursor, url, false).unwrap());
    let broken = temp.path().join("broken.json");
    fs::write(&broken, b"invalid").unwrap();
    assert!(
        install_file(&broken, McpTarget::Cursor, url, false)
            .unwrap_err()
            .contains("--print")
    );
    assert_eq!(fs::read(&broken).unwrap(), b"invalid");
}

#[test]
fn mcp_install_claude_uses_injected_cli_and_project_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let fake = temp.path().join("claude");
    fs::write(
        &fake,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"${0%/*}/calls\"\nif [ \"$2\" = get ]; then exit 1; fi\n",
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let url = "http://127.0.0.1:39601/mcp";
    install_claude_with_program(&fake, McpScope::User, url, false, temp.path(), None).unwrap();
    let calls = fs::read_to_string(temp.path().join("calls")).unwrap();
    assert!(calls.contains("mcp\nadd\n--transport\nhttp\n--scope\nuser\nlantern\n"));
    let project = temp.path().join("project/.mcp.json");
    install_claude_with_program(
        &temp.path().join("missing"),
        McpScope::Project,
        url,
        false,
        temp.path(),
        Some(&project),
    )
    .unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&fs::read(project).unwrap()).unwrap();
    assert_eq!(doc["mcpServers"]["lantern"]["type"], "http");
    assert_eq!(doc["mcpServers"]["lantern"]["url"], url);
}

#[tokio::test]
async fn mcp_install_print_never_writes_injected_path() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("untouched.toml");
    let cli = Cli::try_parse_from([
        "lantern",
        "--host",
        "http://127.0.0.1:39601",
        "mcp",
        "install",
        "codex",
        "--print",
        "--config-path",
        config.to_str().unwrap(),
    ])
    .unwrap();
    let mut out = Vec::new();
    let mut err = Vec::new();
    assert!(matches!(
        dispatch(cli, &UnassembledSystem, &mut out, &mut err, false).await,
        CliAction::Exit(0)
    ));
    let output = String::from_utf8(out).unwrap();
    assert!(
        output.contains("http://127.0.0.1:39601/mcp")
            && output.contains("tool_timeout_sec = 120")
            && output.contains("Claude Code")
    );
    assert!(!config.exists());
}
