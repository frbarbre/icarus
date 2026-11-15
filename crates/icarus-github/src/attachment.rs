use crate::client::{GitHub, http_error};
use crate::error::{GitHubError, Result};
use crate::http::encode_component;
use crate::runner::{Gh, GhRequest, Method};
use icarus_rules::facts::RepoRef;
use serde::Deserialize;

pub const UPLOAD_ENDPOINT: &str = "https://uploads.github.com/user-attachments/assets";
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_VIDEO_BYTES: usize = 100 * 1024 * 1024;

const IMAGE_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];
const VIDEO_TYPES: &[&str] = &["video/mp4", "video/quicktime", "video/webm"];

/// The content type GitHub attachments accept for a file name, from its extension.
pub fn content_type_for(name: &str) -> Option<&'static str> {
    let extension = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        _ => return None,
    })
}

pub fn is_video(content_type: &str) -> bool {
    VIDEO_TYPES.contains(&content_type)
}

/// What to put in a comment: an image link, or a video's bare URL on its own line, which GitHub plays.
pub fn attachment_markdown(name: &str, content_type: &str, url: &str) -> String {
    if is_video(content_type) {
        format!("\n{url}\n")
    } else {
        let alt: String = name
            .chars()
            .filter(|c| !matches!(c, '[' | ']' | '\n'))
            .collect();
        format!("![{alt}]({url})")
    }
}

pub fn check_attachment(content_type: &str, size: usize) -> Result<()> {
    let (kind, limit) = if IMAGE_TYPES.contains(&content_type) {
        ("image", MAX_IMAGE_BYTES)
    } else if VIDEO_TYPES.contains(&content_type) {
        ("video", MAX_VIDEO_BYTES)
    } else {
        return Err(GitHubError::Invalid(
            "Only images and videos can be attached.".to_owned(),
        ));
    };
    if size > limit {
        return Err(GitHubError::Invalid(format!(
            "GitHub takes {kind}s up to {} MB.",
            limit / (1024 * 1024)
        )));
    }
    Ok(())
}

#[derive(Deserialize)]
struct Permissions {
    #[serde(default)]
    push: bool,
}

#[derive(Deserialize)]
struct Repository {
    id: u64,
    #[serde(default)]
    permissions: Option<Permissions>,
}

#[derive(Deserialize)]
struct Uploaded {
    #[serde(default)]
    url: Option<String>,
}

impl<G: Gh> GitHub<G> {
    // Why gh api with an absolute uploads URL: gh attaches its own token for *.github.com hosts, so we never handle it.
    pub async fn upload_attachment(
        &self,
        repo: &RepoRef,
        name: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> Result<String> {
        check_attachment(content_type, bytes.len())?;
        let response = self
            .send(GhRequest::get(format!(
                "repos/{}/{}",
                repo.owner, repo.name
            )))
            .await?;
        if response.status == 404 {
            return Err(GitHubError::NotFound(format!(
                "GitHub cannot find {} for your login.",
                repo.slug()
            )));
        }
        if !response.is_success() {
            return Err(http_error(&response));
        }
        let repository: Repository = response.json()?;
        if !repository
            .permissions
            .is_some_and(|permissions| permissions.push)
        {
            return Err(GitHubError::Invalid(format!(
                "Attaching files needs write access to {}.",
                repo.slug()
            )));
        }
        let endpoint = format!(
            "{UPLOAD_ENDPOINT}?name={}&content_type={}&repository_id={}",
            encode_component(name),
            encode_component(content_type),
            repository.id
        );
        let response = self
            .send(GhRequest::new(Method::Post, endpoint).raw(bytes, content_type))
            .await?;
        if response.status == 404 {
            return Err(GitHubError::Invalid(
                "GitHub refused the upload: attaching files needs write access to the repository."
                    .to_owned(),
            ));
        }
        if !response.is_success() {
            return Err(http_error(&response));
        }
        response
            .json::<Uploaded>()?
            .url
            .filter(|url| url.starts_with("https://"))
            .ok_or_else(|| GitHubError::Malformed("GitHub returned no attachment link.".to_owned()))
    }
}
