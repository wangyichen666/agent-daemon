use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};

use tracing::warn;

const MAX_SKILL_FILES: usize = 64;
const MAX_SKILL_BYTES: usize = 64 * 1024;
const MAX_MATCHES: usize = 3;

pub struct SkillLibrary {
    directory: PathBuf,
}

impl SkillLibrary {
    pub fn from_env(workspace: &Path) -> Self {
        let configured = env::var_os("SKILLS_DIR").map(PathBuf::from);
        let directory = match configured {
            Some(path) if path.is_absolute() => path,
            Some(path) => workspace.join(path),
            None => workspace.join(".my-agent/skills"),
        };
        Self { directory }
    }

    pub async fn context_for(&self, query: &str) -> Option<String> {
        let directory = self.directory.clone();
        let loaded = tokio::task::spawn_blocking(move || load_skills(&directory)).await;
        let skills = match loaded {
            Ok(skills) => skills,
            Err(error) => {
                warn!(%error, "加载 skill 索引的后台任务异常终止");
                return None;
            }
        };
        if skills.is_empty() {
            return None;
        }

        let mut output = String::from("可用技能索引（仅标题与摘要）：");
        for skill in &skills {
            output.push_str(&format!(
                "\n- {} · {}：{}",
                skill.id, skill.title, skill.summary
            ));
        }

        let query_terms = terms(query);
        if query_terms.is_empty() {
            return Some(output);
        }
        let query_lower = query.to_lowercase();
        let mut ranked = skills
            .iter()
            .filter_map(|skill: &Skill| {
                let metadata = format!("{} {} {}", skill.id, skill.title, skill.summary);
                let overlap = terms(&metadata).intersection(&query_terms).count();
                let metadata_lower = metadata.to_lowercase();
                let substring_bonus = usize::from(
                    !query_lower.is_empty()
                        && (metadata_lower.contains(&query_lower)
                            || query_lower.contains(&metadata_lower)),
                );
                let score = overlap.saturating_add(substring_bonus.saturating_mul(100));
                (score > 0).then_some((score, skill))
            })
            .collect::<Vec<(usize, &Skill)>>();
        ranked.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.id.cmp(&right.1.id))
        });
        if !ranked.is_empty() {
            output.push_str("\n\n按当前请求命中的技能正文：");
            for (_, skill) in ranked.into_iter().take(MAX_MATCHES) {
                output.push_str(&format!(
                    "\n\n### {} · {}\n{}",
                    skill.id, skill.title, skill.body
                ));
            }
        }
        Some(output)
    }
}

struct Skill {
    id: String,
    title: String,
    summary: String,
    body: String,
}

fn load_skills(directory: &Path) -> Vec<Skill> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry: std::fs::DirEntry| entry.path())
        .filter(|path: &PathBuf| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        })
        .collect::<Vec<PathBuf>>();
    paths.sort();
    paths
        .into_iter()
        .take(MAX_SKILL_FILES)
        .filter_map(|path: PathBuf| match read_skill(&path) {
            Ok(skill) => Some(skill),
            Err(error) => {
                warn!(path = %path.display(), %error, "跳过无法读取的 skill");
                None
            }
        })
        .collect()
}

fn read_skill(path: &Path) -> std::io::Result<Skill> {
    let bytes = std::fs::read(path)?;
    let limited = if bytes.len() > MAX_SKILL_BYTES {
        &bytes[..MAX_SKILL_BYTES]
    } else {
        &bytes
    };
    let body = String::from_utf8_lossy(limited).into_owned();
    let id = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("skill")
        .to_owned();
    let title = body
        .lines()
        .find_map(|line: &str| line.trim().strip_prefix("# "))
        .map(str::trim)
        .filter(|line: &&str| !line.is_empty())
        .unwrap_or(&id)
        .to_owned();
    let summary = body
        .lines()
        .find_map(|line: &str| {
            line.trim()
                .strip_prefix("summary:")
                .or_else(|| line.trim().strip_prefix("摘要："))
        })
        .map(str::trim)
        .filter(|line: &&str| !line.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| fallback_summary(&body));
    Ok(Skill {
        id,
        title,
        summary,
        body,
    })
}

fn fallback_summary(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|line: &&str| {
            !line.is_empty()
                && !line.starts_with('#')
                && *line != "---"
                && !line.starts_with("summary:")
                && !line.starts_with("摘要：")
        })
        .unwrap_or("（未提供摘要）")
        .chars()
        .take(240)
        .collect()
}

fn terms(text: &str) -> HashSet<String> {
    let mut output = HashSet::new();
    for token in text
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .filter(|token: &&str| token.len() >= 2)
    {
        output.insert(token.to_ascii_lowercase());
    }
    let chinese = text
        .chars()
        .filter(|character: &char| is_cjk(*character))
        .collect::<Vec<char>>();
    for pair in chinese.windows(2) {
        output.insert(pair.iter().collect());
    }
    output
}

fn is_cjk(character: char) -> bool {
    matches!(
        character as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF
    )
}
