use duct_sh::sh_dangerous;
use rig_derive::rig_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::fs;

#[derive(Debug, thiserror::Error, Serialize, Deserialize, JsonSchema)]
pub enum ToolError {
    #[error("IO error: {0}")]
    Io(String),
    #[error("Error: {0}")]
    Generic(String),
}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}
impl From<tokio::task::JoinError> for ToolError {
    fn from(e: tokio::task::JoinError) -> Self {
        Self::Generic(e.to_string())
    }
}

fn get_path(path: &str) -> Result<PathBuf, ToolError> {
    validate_path(
        &std::env::current_dir().map_err(|e| ToolError::Io(e.to_string()))?,
        path,
    )
}

fn validate_path(base: &std::path::Path, path: &str) -> Result<PathBuf, ToolError> {
    let p = std::path::Path::new(path);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };

    let mut result = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::ParentDir => {
                result.pop();
            }
            std::path::Component::CurDir => {}
            c => result.push(c),
        }
    }

    if result.starts_with(base) {
        Ok(result)
    } else {
        Err(ToolError::Generic(
            "Access denied: path must be within the current directory".into(),
        ))
    }
}

fn walk_files(base: &std::path::Path) -> impl Iterator<Item = ignore::DirEntry> {
    ignore::WalkBuilder::new(base)
        .hidden(false)
        .require_git(false)
        .build()
        .filter_map(|r| r.ok())
        .filter(|e| e.file_type().map(|ft| ft.is_file()).unwrap_or(false))
}

#[rig_tool(
    description = "Read file with line numbers",
    required(path, offset, limit)
)]
pub async fn read_file(path: String, offset: u64, limit: u64) -> Result<String, ToolError> {
    let content = fs::read_to_string(get_path(&path)?).await?;
    let lines: Vec<_> = content
        .lines()
        .enumerate()
        .skip(offset as usize)
        .take(if limit == 0 {
            usize::MAX
        } else {
            limit as usize
        })
        .map(|(i, l)| format!("{:4}| {}\n", i + 1, l))
        .collect();
    Ok(lines.concat())
}

#[rig_tool(description = "Write content to file", required(path, content))]
pub async fn write_file(path: String, content: String) -> Result<String, ToolError> {
    fs::write(get_path(&path)?, content).await?;
    Ok("ok".into())
}

#[rig_tool(
    description = "Replace old with new in file (old must be unique unless all=true)",
    required(path, old, new, all)
)]
pub async fn edit_file(
    path: String,
    old: String,
    new: String,
    all: bool,
) -> Result<String, ToolError> {
    let p = get_path(&path)?;
    let text = fs::read_to_string(&p).await?;
    if !text.contains(&old) {
        return Ok("error: old_string not found".into());
    }
    let count = text.matches(&old).count();
    if !all && count > 1 {
        return Ok(format!(
            "error: old_string appears {count} times, must be unique (use all=true)"
        ));
    }
    fs::write(
        p,
        if all {
            text.replace(&old, &new)
        } else {
            text.replacen(&old, &new, 1)
        },
    )
    .await?;
    Ok("ok".into())
}

#[rig_tool(
    description = "Find files by pattern, sorted by mtime",
    required(pat, path)
)]
pub async fn glob_files(pat: String, path: String) -> Result<String, ToolError> {
    let base = get_path(&path)?;
    let matcher = globset::Glob::new(&pat)
        .map_err(|e| ToolError::Generic(e.to_string()))?
        .compile_matcher();
    let entries = tokio::task::spawn_blocking(move || {
        walk_files(&base)
            .filter(|e| matcher.is_match(e.path().strip_prefix(&base).unwrap_or(e.path())))
            .map(|e| e.into_path())
            .collect::<Vec<_>>()
    })
    .await?;

    let mut files = Vec::new();
    for e in entries {
        let mtime = fs::metadata(&e).await.and_then(|m| m.modified()).ok();
        files.push((e, mtime));
    }
    files.sort_by_key(|(_, m)| std::cmp::Reverse(*m));
    let res = files
        .iter()
        .map(|(f, _)| f.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(if res.is_empty() { "none".into() } else { res })
}

#[rig_tool(description = "Search files for regex pattern", required(pat, path))]
pub async fn grep_text(pat: String, path: String) -> Result<String, ToolError> {
    let base = get_path(&path)?;
    let re = regex::Regex::new(&pat).map_err(|e| ToolError::Generic(e.to_string()))?;
    let hits = tokio::task::spawn_blocking(move || {
        walk_files(&base)
            .flat_map(|e| {
                let p = e.path().to_owned();
                std::fs::read_to_string(&p).ok().map(|c| (p, c))
            })
            .flat_map(|(p, c)| {
                let re = re.clone();
                let p_str = p.display().to_string();
                c.lines()
                    .enumerate()
                    .filter(move |(_, l)| re.is_match(l))
                    .map(move |(i, l)| format!("{}:{}:{}", p_str, i + 1, l))
                    .collect::<Vec<_>>()
            })
            .take(50)
            .collect::<Vec<_>>()
    })
    .await?;
    Ok(if hits.is_empty() {
        "none".into()
    } else {
        hits.join("\n")
    })
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct BashArgs {
    pub cmd: String,
}

pub struct Bash;

impl rig_core::tool::Tool for Bash {
    type Args = BashArgs;
    type Output = String;
    type Error = ToolError;

    const NAME: &'static str = "bash";

    async fn definition(&self, _prompt: String) -> rig_core::completion::ToolDefinition {
        rig_core::completion::ToolDefinition {
            name: Self::NAME.into(),
            description: "Run shell command".into(),
            parameters: serde_json::to_value(schemars::schema_for!(BashArgs)).unwrap(),
        }
    }

    async fn call(&self, args: Self::Args) -> std::result::Result<Self::Output, Self::Error> {
        let output = tokio::task::spawn_blocking(move || {
            sh_dangerous(&args.cmd)
                .stderr_to_stdout()
                .unchecked()
                .read()
                .map_err(|e| ToolError::Io(e.to_string()))
        })
        .await??;

        let res = output.trim().to_string();
        Ok(if res.is_empty() {
            "(empty)".into()
        } else {
            res
        })
    }
}

#[rig_tool(description = "List files and directories in a path", required(path))]
pub async fn list_dir(path: String) -> Result<String, ToolError> {
    let base = get_path(&path)?;

    let entries = tokio::task::spawn_blocking(move || {
        ignore::WalkBuilder::new(&base)
            .hidden(false)
            .require_git(false)
            .max_depth(Some(1))
            .build()
            .filter_map(|r| r.ok())
            .filter(|e| e.depth() > 0) // Skip the root directory itself
            .map(|e| {
                let name = e.file_name().to_string_lossy();
                let is_dir = e.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
                format!("{}{}", name, if is_dir { "/" } else { "" })
            })
            .collect::<Vec<String>>()
    })
    .await?;

    let mut res = entries;
    res.sort();
    Ok(if res.is_empty() {
        "(empty)".into()
    } else {
        res.join("\n")
    })
}

#[rig_tool(
    description = "Create a directory (including parent directories)",
    required(path)
)]
pub async fn make_dir(path: String) -> Result<String, ToolError> {
    fs::create_dir_all(get_path(&path)?).await?;
    Ok("ok".into())
}

#[rig_tool(description = "Remove a file or directory", required(path, recursive))]
pub async fn remove(path: String, recursive: bool) -> Result<String, ToolError> {
    let p = get_path(&path)?;
    if p.is_dir() {
        if recursive {
            fs::remove_dir_all(p).await?;
        } else {
            fs::remove_dir(p).await?;
        }
    } else {
        fs::remove_file(p).await?;
    }
    Ok("ok".into())
}

#[rig_tool(description = "Move or rename a file or directory", required(src, dst))]
pub async fn move_file(src: String, dst: String) -> Result<String, ToolError> {
    fs::rename(get_path(&src)?, get_path(&dst)?).await?;
    Ok("ok".into())
}

#[rig_tool(
    description = "Copy a file (does not support directories yet)",
    required(src, dst)
)]
pub async fn copy_file(src: String, dst: String) -> Result<String, ToolError> {
    fs::copy(get_path(&src)?, get_path(&dst)?).await?;
    Ok("ok".into())
}

#[rig_tool(
    description = "Browser automation CLI for AI agents.
Core workflow:
1. Navigate: agent-browser open <url>
2. Snapshot: agent-browser snapshot -i (returns elements with refs like @e1, @e2)
3. Interact: click @e1, fill @e2 \"text\", etc.
4. Re-snapshot after navigation or significant DOM changes

Commands:
- Navigation: open <url>, back, forward, reload, close
- Snapshot: snapshot (full tree), snapshot -i (interactive only), snapshot -c (compact)
- Interactions: click, dblclick, fill, type, press <key>, hover, check, uncheck, select, scroll, scrollintoview
- Information: get text, get value, get title, get url
- Screenshots: screenshot [path] [--full]
- Wait: wait @e1, wait <ms>, wait --text <text>, wait --load networkidle
- Sessions: --session <name> (parallel browsers)
- Output: Add --json for machine-readable output",
    required(args)
)]
pub async fn agent_browser(args: String) -> Result<String, ToolError> {
    let cmd = format!("agent-browser {}", args);
    let output = tokio::task::spawn_blocking(move || {
        sh_dangerous(&cmd)
            .stderr_to_stdout()
            .unchecked()
            .read()
            .map_err(|e| ToolError::Io(e.to_string()))
    })
    .await??;

    let res = output.trim().to_string();
    Ok(if res.is_empty() {
        "(empty)".into()
    } else {
        res
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_validate_path_normal() {
        let base = Path::new("/work");
        assert_eq!(
            validate_path(base, "file.txt").unwrap(),
            Path::new("/work/file.txt")
        );
        assert_eq!(
            validate_path(base, "subdir/file.txt").unwrap(),
            Path::new("/work/subdir/file.txt")
        );
    }

    #[test]
    fn test_validate_path_current_dir() {
        let base = Path::new("/work");
        assert_eq!(validate_path(base, ".").unwrap(), Path::new("/work"));
        assert_eq!(
            validate_path(base, "./file.txt").unwrap(),
            Path::new("/work/file.txt")
        );
    }

    #[test]
    fn test_validate_path_escape_parent() {
        let base = Path::new("/work");
        assert!(validate_path(base, "..").is_err());
        assert!(validate_path(base, "../../etc/passwd").is_err());
    }

    #[test]
    fn test_validate_path_stay_in_bounds() {
        let base = Path::new("/work");
        assert_eq!(
            validate_path(base, "subdir/../file.txt").unwrap(),
            Path::new("/work/file.txt")
        );
        assert_eq!(
            validate_path(base, "subdir/./file.txt").unwrap(),
            Path::new("/work/subdir/file.txt")
        );
    }

    #[test]
    fn test_validate_path_absolute() {
        let base = Path::new("/work");
        // Absolute paths should be allowed if they are inside base
        assert_eq!(
            validate_path(base, "/work/file.txt").unwrap(),
            Path::new("/work/file.txt")
        );
        assert!(validate_path(base, "/etc/passwd").is_err());
    }

    #[test]
    fn test_validate_path_empty() {
        let base = Path::new("/work");
        assert_eq!(validate_path(base, "").unwrap(), Path::new("/work"));
    }

    #[test]
    fn test_validate_path_unforgiving_edge_cases() {
        let base = Path::new("/work");

        // Trying to be clever with many dots
        assert!(validate_path(base, "subdir/../../outside").is_err());

        // Symlink-like behavior (though validate_path doesn't resolve actual symlinks, just components)
        // If we have a path that looks like it's escaping but it's not
        assert_eq!(
            validate_path(base, "a/b/../../c").unwrap(),
            Path::new("/work/c")
        );

        // Path that starts with many slashes
        assert!(validate_path(base, "///etc/passwd").is_err());

        // Path that is just dots - "..." is a valid filename but ".." is not allowed to escape
        assert_eq!(validate_path(base, "...").unwrap(), Path::new("/work/..."));

        // Root path
        assert!(validate_path(base, "/").is_err());
    }
}
