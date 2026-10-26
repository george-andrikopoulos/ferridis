//! Reading the static bearer from a file (`--bearer-token-file`).
//!
//! A service manager that launches `ferridis-mcp-server` with
//! `--bearer-token` or `FERRIDIS_MCP_BEARER` has to hold the token in its
//! own configuration, which is usually a plain-text file readable by more
//! than the service account. Naming a file instead lets that configuration
//! carry a path, and the token lives in one file whose permissions the
//! operator controls.
//!
//! Errors name the path and, where useful, a length. They never carry the
//! token or any part of it, because these messages reach stderr and the
//! service log.

use std::io;
use std::path::{Path, PathBuf};

use secrecy::SecretString;

/// Why a bearer file could not be used. No variant holds file content.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BearerFileError {
    #[error("--bearer-token-file {}: {source}", path.display())]
    Read { path: PathBuf, source: io::Error },

    #[error("--bearer-token-file {}: the file holds no token", path.display())]
    Empty { path: PathBuf },

    #[error(
        "--bearer-token-file {}: the token contains whitespace ({len} characters after trimming); \
         expected one token on one line",
        path.display()
    )]
    InnerWhitespace { path: PathBuf, len: usize },
}

/// Read a bearer token from `path`.
///
/// Surrounding whitespace is trimmed, so a file written by `echo` or an
/// editor (trailing `\n` or `\r\n`) works. Whitespace inside the token is
/// refused: it means the file holds something other than one token, and a
/// CR or LF would also be invalid in an `Authorization` header.
#[must_use = "the token is the server's only static credential"]
pub(crate) fn read_bearer_file(path: &Path) -> Result<SecretString, BearerFileError> {
    let raw = std::fs::read_to_string(path).map_err(|source| BearerFileError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let token = raw.trim();
    if token.is_empty() {
        return Err(BearerFileError::Empty {
            path: path.to_path_buf(),
        });
    }
    if token.chars().any(char::is_whitespace) {
        return Err(BearerFileError::InnerWhitespace {
            path: path.to_path_buf(),
            len: token.chars().count(),
        });
    }
    Ok(SecretString::new(token.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    fn file_with(content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bearer.token");
        std::fs::write(&path, content).expect("write token file");
        (dir, path)
    }

    #[test]
    fn a_trailing_newline_is_trimmed() {
        for content in ["tok-123\n", "tok-123\r\n", "  tok-123  "] {
            let (_dir, path) = file_with(content);
            let token = read_bearer_file(&path).expect("token reads");
            assert_eq!(token.expose_secret(), "tok-123", "{content:?}");
        }
    }

    #[test]
    fn an_empty_or_blank_file_is_refused() {
        for content in ["", "\r\n", "   \n"] {
            let (_dir, path) = file_with(content);
            assert!(
                matches!(read_bearer_file(&path), Err(BearerFileError::Empty { .. })),
                "{content:?}"
            );
        }
    }

    #[test]
    fn whitespace_inside_the_token_is_refused_without_echoing_it() {
        let (_dir, path) = file_with("first-secret-half second-secret-half\n");
        let err = read_bearer_file(&path).expect_err("two words must be refused");
        assert!(matches!(
            err,
            BearerFileError::InnerWhitespace { len: 36, .. }
        ));
        let message = err.to_string();
        assert!(
            !message.contains("secret-half"),
            "error echoed the token: {message}"
        );
    }

    #[test]
    fn a_missing_file_is_a_read_error_naming_the_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("absent.token");
        let err = read_bearer_file(&path).expect_err("missing file must be refused");
        assert!(matches!(err, BearerFileError::Read { .. }));
        assert!(err.to_string().contains("absent.token"));
    }
}
