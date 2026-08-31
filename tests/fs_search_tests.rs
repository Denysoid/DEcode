use std::error::Error;

use decode::tools::{
    ListDirectoryOptions, MAX_READ_FILE_BYTES, SandboxRoot, SearchCodeOptions, SearchError,
    ToolError, list_directory_with_options, read_file, search_code, search_code_with_options,
    write_file,
};
use tempfile::TempDir;

#[tokio::test]
async fn read_file_rejects_invalid_utf8() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;

    std::fs::write(root.path().join("invalid.bin"), [0xff, 0xfe])?;

    let sandbox = SandboxRoot::open(root.path())?;
    let result = read_file(&sandbox, "invalid.bin").await;

    assert!(matches!(result, Err(ToolError::InvalidUtf8 { .. })));

    Ok(())
}

#[tokio::test]
async fn read_file_enforces_size_limit() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let oversized = vec![b'x'; MAX_READ_FILE_BYTES + 1];

    std::fs::write(root.path().join("large.txt"), oversized)?;

    let sandbox = SandboxRoot::open(root.path())?;
    let result = read_file(&sandbox, "large.txt").await;

    assert!(matches!(
        result,
        Err(ToolError::Sandbox(
            decode::tools::SandboxError::FileTooLarge { .. }
        ))
    ));

    Ok(())
}

#[tokio::test]
async fn privacy_shield_blocks_direct_and_walk_based_secret_access() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    std::fs::write(root.path().join(".env"), "TOKEN=never-send")?;
    std::fs::write(root.path().join(".env.example"), "TOKEN=example")?;
    std::fs::write(root.path().join("visible.txt"), "never-send")?;

    let sandbox = SandboxRoot::open(root.path())?;
    let direct = read_file(&sandbox, ".env").await;
    assert!(matches!(
        direct,
        Err(ToolError::Sandbox(decode::tools::SandboxError::Privacy(_)))
    ));
    assert!(read_file(&sandbox, ".env.example").await.is_ok());

    let listing =
        list_directory_with_options(&sandbox, ".", ListDirectoryOptions::default()).await?;
    assert!(!listing.lines().any(|line| line == ".env"));
    assert!(listing.contains(".env.example"));

    let search = search_code(&sandbox, "TOKEN=never-send", None).await?;
    assert!(search.contains("no matches found"));
    Ok(())
}

#[tokio::test]
async fn write_file_atomically_overwrites_regular_file() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let file = root.path().join("file.txt");

    std::fs::write(&file, "old")?;

    let sandbox = SandboxRoot::open(root.path())?;

    write_file(&sandbox, "file.txt", "new").await?;

    assert_eq!(std::fs::read_to_string(file)?, "new");

    Ok(())
}

#[tokio::test]
async fn list_directory_marks_truncated_output() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;

    for index in 0..10 {
        std::fs::write(root.path().join(format!("file-{index}.txt")), "content")?;
    }

    let sandbox = SandboxRoot::open(root.path())?;

    let output = list_directory_with_options(
        &sandbox,
        ".",
        ListDirectoryOptions {
            max_depth: 2,
            max_entries: 1,
            max_output_bytes: 128,
        },
    )
    .await?;

    assert!(output.contains("[directory listing truncated]"));
    assert!(output.len() <= 128);

    Ok(())
}

#[tokio::test]
async fn list_directory_marks_the_per_directory_safety_cap_as_truncated()
-> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    for index in 0..4_100 {
        std::fs::write(root.path().join(format!("file-{index:04}.txt")), "")?;
    }
    let sandbox = SandboxRoot::open(root.path())?;

    let output = list_directory_with_options(
        &sandbox,
        ".",
        ListDirectoryOptions {
            max_depth: 1,
            max_entries: 5_000,
            max_output_bytes: 1_000_000,
        },
    )
    .await?;

    assert!(output.contains("[directory listing truncated]"));
    Ok(())
}

#[tokio::test]
async fn list_directory_rejects_direct_excluded_directory() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;

    std::fs::create_dir(root.path().join("target"))?;

    let sandbox = SandboxRoot::open(root.path())?;
    let result =
        list_directory_with_options(&sandbox, "target", ListDirectoryOptions::default()).await;

    assert!(matches!(
        result,
        Err(ToolError::ExcludedPath {
            operation: "list_directory",
            ..
        })
    ));

    Ok(())
}

#[tokio::test]
async fn search_finds_regex_matches() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;

    std::fs::write(root.path().join("lib.rs"), "fn alpha() {}\nfn beta() {}\n")?;

    let sandbox = SandboxRoot::open(root.path())?;
    let output = search_code(&sandbox, r"fn\s+\w+", None).await?;

    assert!(output.contains("lib.rs:1:1"));
    assert!(output.contains("lib.rs:2:1"));

    Ok(())
}

#[tokio::test]
async fn search_rejects_invalid_regex() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let sandbox = SandboxRoot::open(root.path())?;

    let result = search_code(&sandbox, "(", None).await;

    assert!(matches!(
        result,
        Err(ToolError::Search(SearchError::InvalidRegex { .. }))
    ));

    Ok(())
}

#[tokio::test]
async fn search_respects_gitignore() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;

    std::fs::write(root.path().join(".gitignore"), "ignored.txt\n")?;
    std::fs::write(root.path().join("ignored.txt"), "unique-secret")?;
    std::fs::write(root.path().join("visible.txt"), "ordinary")?;

    let sandbox = SandboxRoot::open(root.path())?;
    let output = search_code(&sandbox, "unique-secret", None).await?;

    assert!(output.contains("no matches found"));
    assert!(!output.contains("ignored.txt:"));

    Ok(())
}

#[tokio::test]
async fn capability_walk_applies_ancestor_and_nested_ignore_rules() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let nested = root.path().join("nested");
    std::fs::create_dir(&nested)?;
    std::fs::write(root.path().join(".gitignore"), "nested/from-root.txt\n")?;
    std::fs::write(nested.join(".ignore"), "*.secret\n")?;
    std::fs::write(nested.join("from-root.txt"), "needle")?;
    std::fs::write(nested.join("local.secret"), "needle")?;
    std::fs::write(nested.join("visible.txt"), "needle")?;

    let sandbox = SandboxRoot::open(root.path())?;
    let output = search_code(&sandbox, "needle", Some("nested")).await?;

    assert!(output.contains("nested\\visible.txt") || output.contains("nested/visible.txt"));
    assert!(!output.contains("from-root.txt:"));
    assert!(!output.contains("local.secret:"));

    let listing =
        list_directory_with_options(&sandbox, "nested", ListDirectoryOptions::default()).await?;
    assert!(!listing.contains("from-root.txt"));
    assert!(!listing.contains("local.secret"));
    assert!(listing.contains("visible.txt"));
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn capability_walk_matches_ignore_rules_case_insensitively_on_windows()
-> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    std::fs::write(root.path().join(".gitignore"), "ignored.txt\n")?;
    std::fs::write(root.path().join("IGNORED.TXT"), "hidden needle")?;
    std::fs::write(root.path().join("visible.txt"), "visible needle")?;
    let sandbox = SandboxRoot::open(root.path())?;

    let listing =
        list_directory_with_options(&sandbox, ".", ListDirectoryOptions::default()).await?;
    let search = search_code(&sandbox, "needle", None).await?;

    assert!(!listing.contains("IGNORED.TXT"));
    assert!(!search.contains("IGNORED.TXT"));
    assert!(search.contains("visible.txt"));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn capability_walk_never_follows_file_symlinks() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::symlink;

    let root = TempDir::new()?;
    let outside = TempDir::new()?;
    std::fs::write(outside.path().join("secret.txt"), "outside-needle")?;
    symlink(
        outside.path().join("secret.txt"),
        root.path().join("link.txt"),
    )?;

    let sandbox = SandboxRoot::open(root.path())?;
    let output = search_code(&sandbox, "outside-needle", None).await?;

    assert!(output.contains("no matches found"));
    assert!(!output.contains("link.txt:"));
    Ok(())
}

#[tokio::test]
async fn search_skips_binary_files() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;

    std::fs::write(root.path().join("binary.bin"), b"prefix\0needle")?;

    let sandbox = SandboxRoot::open(root.path())?;
    let output = search_code(&sandbox, "needle", None).await?;

    assert!(output.contains("no matches found"));
    assert!(output.contains("binary/non-UTF-8 files skipped: 1"));

    Ok(())
}

#[tokio::test]
async fn search_from_root_has_no_fake_inaccessible_candidate() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;

    std::fs::write(root.path().join("visible.txt"), "needle")?;

    let sandbox = SandboxRoot::open(root.path())?;
    let output = search_code(&sandbox, "needle", None).await?;

    assert!(!output.contains("inaccessible or changed candidates: 1"));

    Ok(())
}

#[tokio::test]
async fn search_reports_unicode_column_by_characters() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;

    std::fs::write(root.path().join("unicode.txt"), "абв needle\n")?;

    let sandbox = SandboxRoot::open(root.path())?;
    let output = search_code(&sandbox, "needle", None).await?;

    assert!(output.contains("unicode.txt:1:5"));

    Ok(())
}

#[tokio::test]
async fn search_marks_match_limit_truncation() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;

    std::fs::write(root.path().join("many.txt"), "needle\nneedle\nneedle\n")?;

    let sandbox = SandboxRoot::open(root.path())?;
    let output = search_code_with_options(
        &sandbox,
        "needle",
        None,
        SearchCodeOptions {
            max_matches: 1,
            max_output_bytes: 256,
            max_file_bytes: 1024,
            max_depth: 4,
        },
    )
    .await?;

    assert!(output.contains("[search results truncated]"));
    assert!(output.len() <= 256);

    Ok(())
}

#[tokio::test]
async fn search_rejects_direct_excluded_directory() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;

    std::fs::create_dir(root.path().join(".git"))?;
    std::fs::write(root.path().join(".git/config"), "needle")?;

    let sandbox = SandboxRoot::open(root.path())?;
    let result = search_code(&sandbox, "needle", Some(".git")).await;

    assert!(matches!(
        result,
        Err(ToolError::ExcludedPath {
            operation: "search_code",
            ..
        })
    ));

    Ok(())
}
