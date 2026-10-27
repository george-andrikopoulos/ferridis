//! Reading a secret from a file: the static bearer (`--bearer-token-file`)
//! and the Anthropic API key for `ferridis_ptc_run` (`--ptc-api-key-file`).
//!
//! A service manager that launches `ferridis-mcp-server` with a secret on
//! the command line or in its environment has to hold that secret in its
//! own configuration, which is usually a plain-text file readable by more
//! than the service account. Naming a file instead lets that configuration
//! carry a path, and the secret lives in one file whose permissions the
//! operator controls.
//!
//! Errors name the flag, the path and, where useful, a length. They never
//! carry the secret or any part of it, because these messages reach stderr
//! and the service log.

use std::io;
use std::path::{Path, PathBuf};

use secrecy::SecretString;

/// Which flag named the file. Every error says which, so an operator with
/// two secret files can tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SecretFlag {
    /// `--bearer-token-file`: the static bearer HTTP clients present.
    BearerTokenFile,
    /// `--ptc-api-key-file`: the Anthropic API key `ferridis_ptc_run` uses.
    PtcApiKeyFile,
}

impl std::fmt::Display for SecretFlag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::BearerTokenFile => "--bearer-token-file",
            Self::PtcApiKeyFile => "--ptc-api-key-file",
        })
    }
}

/// Why a secret file could not be used. No variant holds file content.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SecretFileError {
    #[error("{flag} {}: {source}", path.display())]
    Read {
        flag: SecretFlag,
        path: PathBuf,
        source: io::Error,
    },

    #[error("{flag} {}: the file holds no secret", path.display())]
    Empty { flag: SecretFlag, path: PathBuf },

    #[error(
        "{flag} {}: the secret contains whitespace ({len} characters after trimming); \
         expected one value on one line",
        path.display()
    )]
    InnerWhitespace {
        flag: SecretFlag,
        path: PathBuf,
        len: usize,
    },
}

/// Read the secret `flag` names from `path`.
///
/// Surrounding whitespace is trimmed, so a file written by `echo` or an
/// editor (trailing `\n` or `\r\n`) works. Whitespace inside the secret is
/// refused: it means the file holds something other than one value, and a
/// CR or LF would also be invalid in an HTTP header.
#[must_use = "the secret is a credential the server depends on"]
pub(crate) fn read_secret_file(
    flag: SecretFlag,
    path: &Path,
) -> Result<SecretString, SecretFileError> {
    let raw = std::fs::read_to_string(path).map_err(|source| SecretFileError::Read {
        flag,
        path: path.to_path_buf(),
        source,
    })?;
    let secret = raw.trim();
    if secret.is_empty() {
        return Err(SecretFileError::Empty {
            flag,
            path: path.to_path_buf(),
        });
    }
    if secret.chars().any(char::is_whitespace) {
        return Err(SecretFileError::InnerWhitespace {
            flag,
            path: path.to_path_buf(),
            len: secret.chars().count(),
        });
    }
    Ok(SecretString::new(secret.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    const FLAG: SecretFlag = SecretFlag::BearerTokenFile;

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
            let token = read_secret_file(FLAG, &path).expect("token reads");
            assert_eq!(token.expose_secret(), "tok-123", "{content:?}");
        }
    }

    #[test]
    fn an_empty_or_blank_file_is_refused() {
        for content in ["", "\r\n", "   \n"] {
            let (_dir, path) = file_with(content);
            assert!(
                matches!(
                    read_secret_file(FLAG, &path),
                    Err(SecretFileError::Empty { .. })
                ),
                "{content:?}"
            );
        }
    }

    #[test]
    fn whitespace_inside_the_token_is_refused_without_echoing_it() {
        let (_dir, path) = file_with("first-secret-half second-secret-half\n");
        let err = read_secret_file(FLAG, &path).expect_err("two words must be refused");
        assert!(matches!(
            err,
            SecretFileError::InnerWhitespace { len: 36, .. }
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
        let err = read_secret_file(FLAG, &path).expect_err("missing file must be refused");
        assert!(matches!(err, SecretFileError::Read { .. }));
        assert!(err.to_string().contains("absent.token"));
    }

    #[test]
    fn every_error_names_the_flag_that_pointed_at_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("absent.key");
        for (flag, text) in [
            (SecretFlag::BearerTokenFile, "--bearer-token-file"),
            (SecretFlag::PtcApiKeyFile, "--ptc-api-key-file"),
        ] {
            let err = read_secret_file(flag, &path).expect_err("missing file must be refused");
            assert!(err.to_string().starts_with(text), "{err}");
        }
    }
}
