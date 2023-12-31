use std::fs::File;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::anyhow;
use tracing_log::log;

use crate::common::working_dir_path;
use crate::{cmd, cmd_ignore_err};

pub trait GitOperationTrait {
    fn clone_repo(&self, branch: &str) -> anyhow::Result<()>;

    fn create_tag(&self, tag: &str, message: Option<String>) -> anyhow::Result<()>;

    fn delete_tag(&self, tag: &str, force: bool) -> anyhow::Result<()>;

    fn tag_hash(&self, tag: &str) -> anyhow::Result<String>;
    fn previous_tag(&self, tag: &str, is_public: bool) -> anyhow::Result<String>;
}

pub(crate) struct GitRepo {
    pub(crate) owner: String,
    pub(crate) repo: String,

    repo_path: OnceLock<String>,
    repo_dir_path: OnceLock<PathBuf>,
}

impl GitRepo {
    pub(crate) fn new(owner: String, repo: String) -> Self {
        Self {
            owner,
            repo,
            repo_path: Default::default(),
            repo_dir_path: Default::default(),
        }
    }
}

pub(crate) struct GitCli {
    pub(crate) repo: GitRepo,
}

impl GitRepo {
    pub(crate) fn repo_ref(&self) -> &String {
        self.repo_path
            .get_or_init(|| format!("{}/{}", self.owner, self.repo))
    }

    pub(crate) fn repo_dir_path(&self) -> &PathBuf {
        self.repo_dir_path
            .get_or_init(|| working_dir_path().join(&self.repo))
    }
}

impl GitCli {
    pub fn new(owner: String, repo: String) -> Self {
        Self {
            repo: GitRepo::new(owner, repo),
        }
    }
}

impl GitOperationTrait for GitCli {
    fn clone_repo(&self, branch: &str) -> anyhow::Result<()> {
        if self.repo.repo_dir_path().exists() {
            log::info!(
                "Fetching repo {} and reset to branch {}",
                self.repo.repo_ref(),
                branch
            );

            for args in [
                vec!["fetch", "origin", branch],
                vec!["reset", "--hard", &format!("origin/{}", branch)],
                vec!["checkout", branch],
            ] {
                cmd!("git", self.repo.repo_dir_path(), &args);
            }
        } else {
            log::info!("Cloning repo {}", self.repo.repo_ref());

            cmd_ignore_err!(
                "gh",
                working_dir_path(),
                [
                    "repo",
                    "clone",
                    self.repo.repo_ref(),
                    "--",
                    "--branch",
                    branch
                ]
            );
        }

        Ok(())
    }

    fn create_tag(&self, tag: &str, message: Option<String>) -> anyhow::Result<()> {
        if message.is_some() {
            let version_file_path = PathBuf::from(self.repo.repo_ref()).join("version");

            log::info!(
                "Updating the version file {:?} with {}, and making the release commit",
                version_file_path,
                tag
            );

            let mut version_file = File::create(&version_file_path)?;
            version_file.write_all(tag.as_bytes())?;

            let msg = message.unwrap_or(format!("release: {}", tag));
            cmd!(
                "git",
                self.repo.repo_dir_path(),
                ["commit", "-am", &msg, "-s"]
            );
            cmd!("git", self.repo.repo_dir_path(), ["push"]);
        }

        log::info!("Creating tag {}/{}", self.repo.repo_ref(), tag);
        cmd!("git", self.repo.repo_dir_path(), ["tag", tag]);

        log::info!(
            "Pushing tag {}/{} to remote repo",
            self.repo.repo_ref(),
            tag
        );
        cmd!("git", self.repo.repo_dir_path(), ["push", "origin", tag]);

        Ok(())
    }

    fn delete_tag(&self, tag: &str, force: bool) -> anyhow::Result<()> {
        log::info!("Checking if tag {}/{} exists", self.repo.repo_ref(), tag);

        cmd_ignore_err!(
            "git",
            &self.repo.repo_dir_path(),
            ["rev-parse", &format!("refs/tags/{}", tag)],
            {
                if !force {
                    return Err(anyhow!(
                        "Tag {}/{} already exits",
                        self.repo.repo_ref(),
                        tag
                    ));
                }

                log::info!("Deleting existing tag {}/{}", self.repo.repo_ref(), tag);
                cmd!("git", &self.repo.repo_dir_path(), ["tag", "-d", tag]);
                cmd!(
                    "git",
                    &self.repo.repo_dir_path(),
                    ["push", "--delete", "origin", tag]
                );
            }
        );

        Ok(())
    }

    fn tag_hash(&self, tag: &str) -> anyhow::Result<String> {
        let output = cmd!(
            "git",
            &self.repo.repo_dir_path(),
            ["rev-parse", &format!("refs/tags/{}", tag)]
        );

        Ok(String::from_utf8(output.stdout)?.trim_end().to_string())
    }

    fn previous_tag(&self, tag: &str, is_public: bool) -> anyhow::Result<String> {
        let output = cmd!(
            "git",
            &self.repo.repo_dir_path(),
            ["tag", "--sort", "-committerdate"]
        );

        let mut tag_found = false;
        let prev_tag = output.stdout.lines().find(|r| {
            let str = r.as_ref().unwrap();

            if tag_found {
                if is_public {
                    return semver::Version::parse(str.trim_start_matches('v'))
                        .unwrap()
                        .pre
                        .is_empty();
                }

                return true;
            }

            tag_found = str == tag;
            false
        });

        if let Some(prev_tag) = prev_tag {
            Ok(prev_tag?)
        } else {
            Err(anyhow!("tag not found: {}", tag))
        }
    }
}
