use std::error::Error;

use decode::tools::{SandboxError, SandboxRoot, ToolError, read_file, write_file};
use tempfile::TempDir;

#[tokio::test]
async fn rejects_parent_traversal() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let sandbox = SandboxRoot::open(root.path())?;

    let result = read_file(&sandbox, "../../etc/passwd").await;

    assert!(matches!(
        result,
        Err(ToolError::Sandbox(SandboxError::PathEscape { .. }))
    ));

    Ok(())
}

#[tokio::test]
async fn rejects_absolute_path() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let outside = TempDir::new()?;
    let outside_file = outside.path().join("outside.txt");

    std::fs::write(&outside_file, "secret")?;

    let sandbox = SandboxRoot::open(root.path())?;
    let requested = outside_file.to_string_lossy().into_owned();
    let result = read_file(&sandbox, &requested).await;

    assert!(matches!(
        result,
        Err(ToolError::Sandbox(SandboxError::PathEscape { .. }))
    ));

    Ok(())
}

#[tokio::test]
async fn rejects_nul_in_model_path() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let sandbox = SandboxRoot::open(root.path())?;

    let result = read_file(&sandbox, "file\0name").await;

    assert!(matches!(
        result,
        Err(ToolError::Sandbox(SandboxError::PathEscape { .. }))
    ));

    Ok(())
}

#[tokio::test]
async fn write_file_creates_nested_directories() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let sandbox = SandboxRoot::open(root.path())?;

    write_file(&sandbox, "nested/deeper/file.txt", "content").await?;

    assert_eq!(
        std::fs::read_to_string(root.path().join("nested/deeper/file.txt"))?,
        "content"
    );

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_final_symlink_leading_outside() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::symlink;

    let root = TempDir::new()?;
    let outside = TempDir::new()?;
    let outside_file = outside.path().join("outside.txt");

    std::fs::write(&outside_file, "secret")?;
    symlink(&outside_file, root.path().join("link.txt"))?;

    let sandbox = SandboxRoot::open(root.path())?;
    let result = read_file(&sandbox, "link.txt").await;

    assert!(matches!(
        result,
        Err(ToolError::Sandbox(SandboxError::SymlinkForbidden { .. }))
    ));

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn write_rejects_symlink_without_replacing_it() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::symlink;

    let root = TempDir::new()?;
    let outside = TempDir::new()?;
    let outside_file = outside.path().join("outside.txt");
    let link = root.path().join("link.txt");

    std::fs::write(&outside_file, "original")?;
    symlink(&outside_file, &link)?;

    let sandbox = SandboxRoot::open(root.path())?;
    let result = write_file(&sandbox, "link.txt", "replacement").await;

    assert!(matches!(
        result,
        Err(ToolError::Sandbox(SandboxError::SymlinkForbidden { .. }))
    ));
    assert_eq!(std::fs::read_to_string(&outside_file)?, "original");
    assert!(std::fs::symlink_metadata(&link)?.file_type().is_symlink());

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_symlinked_parent_leading_outside() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::symlink;

    let root = TempDir::new()?;
    let outside = TempDir::new()?;

    symlink(outside.path(), root.path().join("external"))?;

    let sandbox = SandboxRoot::open(root.path())?;
    let result = write_file(&sandbox, "external/file.txt", "content").await;

    assert!(matches!(
        result,
        Err(ToolError::Sandbox(SandboxError::SymlinkForbidden { .. }))
    ));
    assert!(!outside.path().join("file.txt").exists());

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_internal_symlinks_by_explicit_policy() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::symlink;

    let root = TempDir::new()?;
    let target = root.path().join("target.txt");

    std::fs::write(&target, "content")?;
    symlink("target.txt", root.path().join("link.txt"))?;

    let sandbox = SandboxRoot::open(root.path())?;
    let result = read_file(&sandbox, "link.txt").await;

    assert!(matches!(
        result,
        Err(ToolError::Sandbox(SandboxError::SymlinkForbidden { .. }))
    ));

    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn rejects_windows_directory_junction_leading_outside() -> Result<(), Box<dyn Error>> {
    use std::process::Command;

    let root = TempDir::new()?;
    let outside = TempDir::new()?;
    std::fs::write(outside.path().join("outside.txt"), "secret")?;
    let junction = root.path().join("junction");
    let output = Command::new("cmd.exe")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(&junction)
        .arg(outside.path())
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "could not create test junction: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }

    let sandbox = SandboxRoot::open(root.path())?;
    let read_result = read_file(&sandbox, "junction/outside.txt").await;
    let write_result = write_file(&sandbox, "junction/new.txt", "forbidden").await;

    assert!(matches!(
        read_result,
        Err(ToolError::Sandbox(SandboxError::SymlinkForbidden { .. }))
    ));
    assert!(matches!(
        write_result,
        Err(ToolError::Sandbox(SandboxError::SymlinkForbidden { .. }))
    ));
    assert!(!outside.path().join("new.txt").exists());
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn rejects_windows_alternate_data_stream_paths() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    std::fs::write(root.path().join(".env"), "TOKEN=secret")?;
    std::fs::write(root.path().join("visible.txt"), "visible")?;
    let sandbox = SandboxRoot::open(root.path())?;

    let read_result = read_file(&sandbox, ".env::$DATA").await;
    let write_result = write_file(&sandbox, "visible.txt:hidden", "hidden").await;

    assert!(matches!(
        read_result,
        Err(ToolError::Sandbox(SandboxError::PathEscape { .. }))
    ));
    assert!(matches!(
        write_result,
        Err(ToolError::Sandbox(SandboxError::PathEscape { .. }))
    ));
    assert_eq!(
        std::fs::read_to_string(root.path().join("visible.txt"))?,
        "visible"
    );
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn windows_path_aliases_cannot_bypass_privacy_rules() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    std::fs::write(root.path().join(".env"), "TOKEN=secret")?;
    let sandbox = SandboxRoot::open(root.path())?;

    for alias in [".ENV", ".env.", ".env ", "NUL"] {
        let result = read_file(&sandbox, alias).await;
        assert!(result.is_err(), "privacy alias was readable: {alias:?}");
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_non_regular_socket_file() -> Result<(), Box<dyn Error>> {
    use std::os::unix::net::UnixListener;

    let root = TempDir::new()?;
    let socket_path = root.path().join("agent.sock");
    let _listener = UnixListener::bind(&socket_path)?;

    let sandbox = SandboxRoot::open(root.path())?;
    let result = read_file(&sandbox, "agent.sock").await;

    assert!(matches!(
        result,
        Err(ToolError::Sandbox(SandboxError::NotRegularFile { .. }))
    ));

    Ok(())
}
