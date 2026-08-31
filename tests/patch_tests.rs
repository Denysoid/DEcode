use std::error::Error;

#[cfg(unix)]
use decode::tools::SandboxError;
use decode::tools::{
    MAX_PATCH_RESULT_BYTES, PatchError, PatchHint, SandboxRoot, ToolError, apply_patch,
};
use tempfile::TempDir;

#[tokio::test]
async fn replaces_exactly_one_occurrence() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let file = root.path().join("file.txt");

    std::fs::write(&file, "before old after")?;

    let sandbox = SandboxRoot::open(root.path())?;

    apply_patch(&sandbox, "file.txt", "old", "new").await?;

    assert_eq!(std::fs::read_to_string(file)?, "before new after");

    Ok(())
}

#[tokio::test]
async fn not_found_does_not_modify_file() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let file = root.path().join("file.txt");
    let original = "unchanged";

    std::fs::write(&file, original)?;

    let sandbox = SandboxRoot::open(root.path())?;
    let result = apply_patch(&sandbox, "file.txt", "missing", "replacement").await;

    assert!(matches!(
        result,
        Err(ToolError::Patch(PatchError::NotFound { .. }))
    ));
    assert_eq!(std::fs::read_to_string(file)?, original);

    Ok(())
}

#[tokio::test]
async fn ambiguous_reports_exact_count_and_does_not_modify() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let file = root.path().join("file.txt");
    let original = "same same same";

    std::fs::write(&file, original)?;

    let sandbox = SandboxRoot::open(root.path())?;
    let result = apply_patch(&sandbox, "file.txt", "same", "different").await;

    assert!(matches!(
        result,
        Err(ToolError::Patch(PatchError::Ambiguous { count: 3, .. }))
    ));
    assert_eq!(std::fs::read_to_string(file)?, original);

    Ok(())
}

#[tokio::test]
async fn overlapping_exact_matches_are_ambiguous() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let file = root.path().join("file.txt");
    std::fs::write(&file, "aaa")?;
    let sandbox = SandboxRoot::open(root.path())?;

    let result = apply_patch(&sandbox, "file.txt", "aa", "x").await;

    assert!(matches!(
        result,
        Err(ToolError::Patch(PatchError::Ambiguous { count: 2, .. }))
    ));
    assert_eq!(std::fs::read_to_string(file)?, "aaa");
    Ok(())
}

#[tokio::test]
async fn patches_unicode_content() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let file = root.path().join("unicode.txt");

    std::fs::write(&file, "Привет, мир 🌍")?;

    let sandbox = SandboxRoot::open(root.path())?;

    apply_patch(&sandbox, "unicode.txt", "мир 🌍", "Rust 🦀").await?;

    assert_eq!(std::fs::read_to_string(file)?, "Привет, Rust 🦀");

    Ok(())
}

#[tokio::test]
async fn preserves_crlf_when_patch_uses_exact_crlf() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let file = root.path().join("crlf.txt");

    std::fs::write(&file, b"first\r\nold\r\nlast\r\n")?;

    let sandbox = SandboxRoot::open(root.path())?;

    apply_patch(&sandbox, "crlf.txt", "old\r\n", "new\r\n").await?;

    assert_eq!(std::fs::read(file)?, b"first\r\nnew\r\nlast\r\n");

    Ok(())
}

#[tokio::test]
async fn lf_search_does_not_silently_patch_crlf_file() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let file = root.path().join("crlf.txt");
    let original = b"first\r\nsecond\r\n";

    std::fs::write(&file, original)?;

    let sandbox = SandboxRoot::open(root.path())?;

    let result = apply_patch(&sandbox, "crlf.txt", "first\nsecond\n", "replacement\n").await;

    assert!(matches!(
        result,
        Err(ToolError::Patch(PatchError::NotFound {
            hint: PatchHint::WhitespaceDifference,
            ..
        }))
    ));
    assert_eq!(std::fs::read(file)?, original);

    Ok(())
}

#[tokio::test]
async fn empty_replace_deletes_exact_match() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let file = root.path().join("file.txt");

    std::fs::write(&file, "before obsolete after")?;

    let sandbox = SandboxRoot::open(root.path())?;

    apply_patch(&sandbox, "file.txt", " obsolete", "").await?;

    assert_eq!(std::fs::read_to_string(file)?, "before after");

    Ok(())
}

#[tokio::test]
async fn empty_search_is_rejected_without_modification() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let file = root.path().join("file.txt");

    std::fs::write(&file, "content")?;

    let sandbox = SandboxRoot::open(root.path())?;
    let result = apply_patch(&sandbox, "file.txt", "", "replacement").await;

    assert!(matches!(
        result,
        Err(ToolError::Patch(PatchError::EmptySearch { .. }))
    ));
    assert_eq!(std::fs::read_to_string(file)?, "content");

    Ok(())
}

#[tokio::test]
async fn invalid_utf8_is_rejected() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let file = root.path().join("binary.dat");

    std::fs::write(&file, [0xff, 0xfe, 0xfd])?;

    let sandbox = SandboxRoot::open(root.path())?;
    let result = apply_patch(&sandbox, "binary.dat", "old", "new").await;

    assert!(matches!(
        result,
        Err(ToolError::Patch(PatchError::InvalidUtf8 { .. }))
    ));

    Ok(())
}

#[tokio::test]
async fn concurrent_patches_do_not_lose_each_others_commits() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let file = root.path().join("file.txt");
    std::fs::write(&file, "alpha beta")?;
    let sandbox = SandboxRoot::open(root.path())?;

    let first = apply_patch(&sandbox, "file.txt", "alpha", "ALPHA");
    let second = apply_patch(&sandbox, "file.txt", "beta", "BETA");
    let (first, second) = tokio::join!(first, second);

    first?;
    second?;
    assert_eq!(std::fs::read_to_string(file)?, "ALPHA BETA");
    Ok(())
}

#[tokio::test]
async fn oversized_patch_result_is_rejected_without_commit() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let file = root.path().join("large.txt");
    let mut original = String::with_capacity(MAX_PATCH_RESULT_BYTES);
    original.push('x');
    original.push_str(&"a".repeat(MAX_PATCH_RESULT_BYTES - 1));
    std::fs::write(&file, &original)?;
    let sandbox = SandboxRoot::open(root.path())?;

    let result = apply_patch(&sandbox, "large.txt", "x", "xx").await;

    assert!(matches!(result, Err(ToolError::InputTooLarge { .. })));
    assert_eq!(std::fs::read_to_string(file)?, original);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn preserves_existing_file_permissions() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;

    let root = TempDir::new()?;
    let file = root.path().join("script.sh");

    std::fs::write(&file, "#!/bin/sh\necho old\n")?;

    let mut permissions = std::fs::metadata(&file)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&file, permissions)?;

    let sandbox = SandboxRoot::open(root.path())?;

    apply_patch(&sandbox, "script.sh", "echo old", "echo new").await?;

    let resulting_mode = std::fs::metadata(file)?.permissions().mode() & 0o777;

    assert_eq!(resulting_mode, 0o755);

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn patch_rejects_symlink_without_destroying_it() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::symlink;

    let root = TempDir::new()?;
    let target = root.path().join("target.txt");
    let link = root.path().join("link.txt");

    std::fs::write(&target, "old")?;
    symlink("target.txt", &link)?;

    let sandbox = SandboxRoot::open(root.path())?;
    let result = apply_patch(&sandbox, "link.txt", "old", "new").await;

    assert!(matches!(
        result,
        Err(ToolError::Sandbox(SandboxError::SymlinkForbidden { .. }))
    ));
    assert_eq!(std::fs::read_to_string(target)?, "old");
    assert!(std::fs::symlink_metadata(link)?.file_type().is_symlink());

    Ok(())
}
