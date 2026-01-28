use std::collections::HashSet;
use std::path::PathBuf;
use std::{fs, vec};

use anyhow::anyhow;
use async_trait::async_trait;
use chrono::{Duration, Utc};
use clap::Args;
use convert_case::{Case, Casing};
use glob::glob_with;
use indexmap::{indexmap, indexset};
use maplit::hashset;
use octocrab::models::issues::Issue;
use octocrab::models::Milestone;
use octocrab::params::issues::Sort;
use octocrab::params::State;
use tracing::log;

use crate::cmds::CliCommand;
use crate::common::{execute, working_dir_path};
use crate::git::{GitCli, GitOperationTrait};
use crate::github::github_client;
use crate::{cmd, Cli};

#[derive(Args)]
#[command(about = "Create a GitHub release")]
pub struct ReleaseArgs {
    #[arg(long, help = "GitHub owner")]
    owner: String,

    #[arg(long, help = "GitHub repo")]
    repo: String,

    #[arg(long, value_delimiter = ',', help = "List of GitHub org/repo where issues are searched")]
    ticket_sources: Option<Vec<String>>,

    #[arg(long, help = "Tag")]
    tag: String,

    #[arg(long, help = "Milestone")]
    milestone: String,

    #[arg(long, help = "Branch")]
    branch: String,

    #[arg(long, help = "Labels to search issues outside the milestone")]
    labels: Option<Vec<String>>,

    #[arg(long, help = "Labels to exclude issues")]
    exclude_labels: Option<Vec<String>>,

    #[arg(long, help = "Release title")]
    note_title: Option<String>,

    #[arg(long, help = "Release pre note file, add before the generated note")]
    pre_note: Option<String>,

    #[arg(long, help = "Release post note file, append after the generated note")]
    post_note: Option<String>,

    #[arg(
        short = 's',
        long,
        help = "Release section labels, for the generated note"
    )]
    note_section_labels: Option<Vec<String>>,

    #[arg(long, help = "Disable release section")]
    note_section_disable: bool,

    #[arg(
        short = 'c',
        long,
        help = "Release extra contributors, for the generated note"
    )]
    note_contributors: Option<Vec<String>>,

    #[arg(
        long,
        default_value = "14",
        help = "Search issues since days for the generated note"
    )]
    since_days: i64,

    #[arg(long, help = "Script to filter searched issues")]
    filter_issue_hook: Option<String>,

    #[arg(long, help = "Files to upload to the release (support glob)")]
    artifacts: Option<Vec<String>>,

    #[arg(long, help = "Dry run")]
    dryrun: bool,

    #[arg(long, help = "Create a draft release")]
    draft: bool,

    #[arg(long, help = "Create a pre release")]
    pre_release: bool,

    #[arg(long, help = "Force to delete the existing tag")]
    force: bool,
}

#[async_trait]
impl CliCommand for ReleaseArgs {
    async fn run(&self, _cli: &Cli) -> anyhow::Result<()> {
        let ticket_sources = self.ticket_sources.clone().unwrap_or_default();
        let mut owners = vec![];
        let mut repos = vec![];

        for source in ticket_sources {
            let parts: Vec<&str> = source.split('/').collect();
            if parts.len() != 2 {
                return Err(anyhow!("Invalid ticket source format: {}", source));
            }
            owners.push(parts[0].to_string());
            repos.push(parts[1].to_string());
        }

        // Collect issues from all owner:repo pairs
        let mut all_issue_ids = HashSet::new();
        let mut all_issues = Vec::new();

        for (owner, repo) in owners.iter().zip(repos.iter()) {
            log::info!("Processing {}/{}", owner, repo);

            let git = GitCli::new(owner.to_string(), repo.to_string());

            git.clone_repo(&self.branch)?;

            if let Err(err) = git.delete_tag(&self.tag, self.force) {
                if !self.force {
                    log::warn!("Skipped to force creating tag {}: {}", self.tag, err);
                }
            }

            let (issue_ids, issues) = self.search_issues(owner, repo).await?;

            all_issue_ids.extend(issue_ids);
            all_issues.extend(issues);
        }

        // Apply filter hook to all collected issues
        if let Some(hook) = &self.filter_issue_hook {
            log::info!("Filtering issues by hook {}", hook);

            let issue_lines = execute(Some(hook), None)?;
            for issue_id in issue_lines.lines() {
                let issue_id = issue_id.parse::<u64>()?;

                if all_issue_ids.remove(&issue_id) {
                    all_issues.retain(|issue| issue.number != issue_id)
                }
            }
        }

        let pre_note = if let Some(p) = &self.pre_note {
            log::info!("Reading pre note file: {}", p);
            read_note_file(p)
        } else {
            String::new()
        };

        let post_note = if let Some(p) = &self.post_note {
            log::info!("Reading post note file: {}", p);
            read_note_file(p)
        } else {
            String::new()
        };

        let note = self.create_release(
            &mut all_issue_ids,
            &all_issues,
            &pre_note,
            &post_note,
            self.note_section_disable,
        )?;

        println!("{}", note);

        Ok(())
    }
}

fn read_note_file(path: impl Into<PathBuf>) -> String {
    let path = path.into();

    if !path.exists() || !path.is_file() {
        log::warn!("{:?} not valid", path);
        return path.to_str().unwrap().to_string();
    }

    fs::read_to_string(path).unwrap_or_default()
}

impl ReleaseArgs {
    async fn search_issues(&self, owner: &str, repo: &str) -> anyhow::Result<(HashSet<u64>, Vec<Issue>)> {
        log::info!("Searching issues for {}/{}", owner, repo);

        let labels = self.labels.clone().unwrap_or_default();
        let exclude_labels = self.exclude_labels.clone().unwrap_or_default();

        let milestones: Vec<Milestone> = github_client()
            .get(
                format!("/repos/{}/{}/milestones", owner, repo),
                None::<&()>,
            )
            .await?;

        let milestone =
            if let Some(milestone) = milestones.iter().find(|m| m.title == self.milestone) {
                milestone
            } else {
                return Err(anyhow!("{} milestone not found", self.milestone));
            };

        log::info!(
            "Searching issues by milestone: {}, labels: {:?}",
            milestone.title,
            labels
        );

        let mut issues: Vec<Issue> = vec![];
        let mut issue_ids = hashset! {};

        let issue_handler = github_client().issues(owner, repo);
        let since_date = Utc::now() - Duration::days(self.since_days);

        for search_type in ["label", "milestone"] {
            let mut page: u32 = 1;

            loop {
                let builder = match search_type {
                    "label" => {
                        if labels.is_empty() {
                            break;
                        }
                        issue_handler.list().labels(&labels).state(State::All)
                    }
                    "milestone" => issue_handler
                        .list()
                        .milestone(milestone.number as u64)
                        .state(State::All),
                    _ => return Err(anyhow!("invalid search type")),
                }
                .sort(Sort::Updated)
                .since(since_date);

                let mut results = builder.page(page).send().await?.items;

                results.retain(|issue| {
                    if let Some(closed_at) = issue.closed_at {
                        if closed_at < since_date {
                            return false;
                        }
                    }

                    !issue
                        .labels
                        .iter()
                        .any(|label| exclude_labels.contains(&label.name))
                });

                if results.is_empty() {
                    break;
                }

                results.iter().for_each(|it| {
                    issue_ids.insert(it.number);
                });

                issues.append(&mut results);
                page += 1;
            }
        }

        Ok((issue_ids, issues))
    }

    fn create_release(
        &self,
        issue_ids: &mut HashSet<u64>,
        issues: &Vec<Issue>,
        pre_note: &String,
        post_note: &String,
        note_section_disable: bool,
    ) -> anyhow::Result<String> {
        log::info!("Creating a release for {}", self.tag);

        let mut note = pre_note.clone();
        let post_note = post_note.clone();
        let mut sections: indexmap::IndexMap<String, Vec<&Issue>> = indexmap! {};
        let mut contributors = indexset! {};

        let note_section_labels = self.note_section_labels.clone().unwrap_or_default();
        let note_contributors = self.note_contributors.clone().unwrap_or_default();

        for contributor in &note_contributors {
            contributors.insert(contributor);
        }

        let get_section_key = |label: &str| {
            if label.contains('/') {
                label[label.rfind('/').unwrap_or_default() + 1..].to_owned()
            } else {
                label.to_owned()
            }
        };

        for label in &note_section_labels {
            sections.insert(get_section_key(label), vec![]);
        }
        sections.insert("misc".to_owned(), vec![]);

        for issue in issues {
            if !issue_ids.contains(&issue.number) {
                continue;
            }
            issue_ids.remove(&issue.number);

            // Add issue author to contributors
            contributors.insert(&issue.user.login);

            for assignee in &issue.assignees {
                contributors.insert(&assignee.login);
            }

            let mut section_key = "misc".to_string();
            for label in &note_section_labels {
                if issue.labels.iter().any(|it| it.name == *label) {
                    section_key = get_section_key(label);
                    break;
                };
            }

            sections.get_mut(&section_key).unwrap().push(issue);
        }

        if note_section_disable {
            sections.swap_remove("misc");
        }

        for (title, issues) in &sections {
            if issues.is_empty() {
                continue;
            }

            note += &format!("\n### {}\n", title.to_case(Case::Title));

            for issue in issues {
                if issue.html_url.to_string().contains("pull") {
                    continue;
                }

                let contributors: Vec<String> =
                    issue.assignees.iter().map(|it| it.login.clone()).collect();
                note += &format!(
                    "- {} [{}]({}) - {}\n",
                    issue.title,
                    issue.number,
                    issue.html_url,
                    contributors
                        .iter()
                        .map(|it| format!("@{it}"))
                        .collect::<Vec<String>>()
                        .join(" ")
                );
            }
        }

        note += post_note.as_str();

        note += "\n## Contributors\n";
        contributors.sort();
        for c in &contributors {
            note += &format!("- @{} \n", c);
        }

        if !self.dryrun {
            let repo_dir_path = working_dir_path().join(&self.repo);
            let release_title = self.note_title.clone().unwrap_or(format!(
                "{} {}",
                self.repo.to_case(Case::Title),
                self.tag
            ));
            let mut args = vec![
                "release".to_string(),
                "create".to_string(),
                self.tag.clone(),
                "--notes".to_string(),
                note.clone(),
                "--target".to_string(),
                self.branch.clone(),
                "--title".to_string(),
                release_title,
            ];

            if self.draft {
                args.push("--draft".to_string());
            }

            if self.pre_release {
                args.push("--prerelease".to_string())
            }

            update_gh_args_from_artifacts(&mut args, self.artifacts.as_ref().unwrap_or(&vec![]));

            cmd!("gh", &repo_dir_path, &args);
        }

        Ok(note)
    }
}

fn update_gh_args_from_artifacts(args: &mut Vec<String>, artifacts: &Vec<String>) {
    for artifact in artifacts {
        glob_with(artifact, glob::MatchOptions::new())
            .unwrap()
            .filter_map(Result::ok)
            .flat_map(|it| it.canonicalize())
            .for_each(|path| {
                args.push(path.to_string_lossy().to_string());
            });
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use filepath::FilePath;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn test_update_gh_args_from_artifacts() -> anyhow::Result<()> {
        let dir = tempdir()?;

        let mut files = vec![];
        for file in vec!["dummy.sbom", "dummy2.sbom"] {
            files.push(
                File::create(dir.path().join(file))?
                    .path()?
                    .canonicalize()?
                    .to_string_lossy()
                    .to_string(),
            );
        }

        let mut args = vec!["non-existing.sbom".to_string()];
        let artifact_globs = vec![format!(
            "{}/*.sbom",
            dir.path().canonicalize()?.to_string_lossy().to_string()
        )];

        update_gh_args_from_artifacts(&mut args, &artifact_globs);

        assert_eq!(args.len() - 1, files.len());
        assert_eq!(args[1..], files);

        Ok(())
    }
}
