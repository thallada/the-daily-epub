//! `daily-epub config check` end to end: the built binary, a temp config, no
//! environment. It must validate like `generate`, print the provider table
//! with `key MISSING` warnings, exit 0, and exit non-zero on an invalid file —
//! all without a database or the run lock.

use std::path::Path;
use std::process::Command;

fn run(config_body: &str) -> (i32, String, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, config_body).expect("write config");
    let output = Command::new(env!("CARGO_BIN_EXE_daily-epub"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .args(["--config"])
        .arg(&path)
        .args(["config", "check"])
        .output()
        .expect("run daily-epub");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn config_check_prints_the_facts_and_exits_zero_without_keys() {
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
    let body = std::fs::read_to_string(example).expect("example config");
    let (code, stdout, stderr) = run(&body);
    assert_eq!(code, 0, "stdout:\n{stdout}\nstderr:\n{stderr}");
    for needle in [
        "config: ",
        "database_path: /var/lib/daily-epub/daily-epub.db",
        "profile_path: data/profile.md",
        "interests_opml: data/scour-interests.opml",
        "llm.bulk: deepseek · openai · deepseek-v4-flash",
        "key MISSING (set DAILY_EPUB_PROVIDERS__DEEPSEEK__API_KEY)",
        "llm.editor: anthropic · anthropic · claude-opus-5 · effort high · max_daily_usd $3.00",
        "key MISSING (set DAILY_EPUB_PROVIDERS__ANTHROPIC__API_KEY)",
        "providers.gemini: unreferenced",
        "voyage: voyage-4-lite · enabled · max_daily_usd $0.50 · key MISSING (set DAILY_EPUB_VOYAGE__API_KEY)",
        "editorial.summary_model: editor",
        "publish.epub_dir: /srv/bookorbit/libraries/daily-epub",
        "publish.xtc_dir: /var/lib/daily-epub/xtc",
    ] {
        assert!(stdout.contains(needle), "missing {needle:?} in:\n{stdout}");
    }
    assert!(
        stdout.lines().any(|line| line.starts_with("! ")),
        "missing keys are flagged with a `!` prefix:\n{stdout}"
    );
    // No lock file, no database: the command is read-only.
    assert!(!Path::new("/var/lib/daily-epub/daily-epub.db.lock").exists());
}

#[test]
fn config_check_exits_non_zero_on_an_invalid_config() {
    let (code, stdout, stderr) = run("[llm]\nbulk = \"nope\"\n");
    assert_ne!(code, 0);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("providers.nope"), "{stderr}");

    let (code, _, stderr) = run("[deepseek]\nmodel = \"x\"\n");
    assert_ne!(code, 0);
    assert!(stderr.contains("[providers.deepseek]"), "{stderr}");
}
