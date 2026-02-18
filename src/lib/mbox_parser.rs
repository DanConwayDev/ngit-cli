use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike};

#[derive(Debug, Clone, PartialEq)]
pub struct PatchMetadata {
    pub commit_id: String,
    pub author_name: String,
    pub author_email: String,
    pub author_timestamp: i64,
    pub author_offset_minutes: i32,
    pub committer_timestamp: Option<i64>,
    pub subject: String,
    pub body: String,
}

pub fn parse_mbox_patch(content: &str) -> Result<PatchMetadata> {
    let commit_id = extract_commit_id_from_mbox(content)?;
    let (author_name, author_email) = extract_author_from_from_header(content)?;
    let (author_timestamp, author_offset_minutes) = extract_date_from_header(content)?;
    let committer_timestamp = extract_committer_date_from_mbox(content)?;
    let subject = extract_subject(content)?;
    let body = extract_commit_message_body(content)?;

    Ok(PatchMetadata {
        commit_id,
        author_name,
        author_email,
        author_timestamp,
        author_offset_minutes,
        committer_timestamp,
        subject,
        body,
    })
}

fn extract_commit_id_from_mbox(content: &str) -> Result<String> {
    if !content.starts_with("From ") {
        bail!("patch does not start with 'From ' - not a valid mbox format");
    }

    let first_line = content.lines().next().context("patch content is empty")?;

    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() < 2 {
        bail!("mbox 'From ' line does not contain a commit id");
    }

    Ok(parts[1].to_string())
}

fn extract_author_from_from_header(content: &str) -> Result<(String, String)> {
    let from_line = content
        .lines()
        .find(|line| line.starts_with("From:"))
        .context("patch does not contain a 'From:' header")?;

    let from_value = from_line
        .strip_prefix("From:")
        .context("failed to strip 'From:' prefix")?
        .trim();

    parse_from_header_value(from_value)
}

fn parse_from_header_value(value: &str) -> Result<(String, String)> {
    if let Some(start) = value.find('<') {
        if let Some(end) = value.find('>') {
            let email = value[start + 1..end].to_string();
            let name_part = value[..start].trim();
            let name = name_part.trim_matches('"').trim().to_string();
            return Ok((name, email));
        }
    }

    if value.contains('@') {
        let email = value.trim().to_string();
        let name = email.split('@').next().unwrap_or("unknown").to_string();
        return Ok((name, email));
    }

    bail!("could not parse From header: {}", value)
}

fn extract_date_from_header(content: &str) -> Result<(i64, i32)> {
    let date_line = content
        .lines()
        .find(|line| line.starts_with("Date:"))
        .context("patch does not contain a 'Date:' header")?;

    let date_value = date_line
        .strip_prefix("Date:")
        .context("failed to strip 'Date:' prefix")?
        .trim();

    parse_rfc2822_date(date_value)
}

fn parse_rfc2822_date(value: &str) -> Result<(i64, i32)> {
    let parsed = DateTime::parse_from_rfc2822(value)
        .context(format!("failed to parse RFC2822 date: {}", value))?;

    let timestamp = parsed.timestamp();
    let offset_minutes = parsed.offset().local_minus_utc() / 60;

    Ok((timestamp, offset_minutes))
}

fn extract_committer_date_from_mbox(content: &str) -> Result<Option<i64>> {
    let first_line = content.lines().next().context("patch content is empty")?;

    let parts: Vec<&str> = first_line.split_whitespace().collect();

    if parts.len() >= 6 {
        let date_str = parts[3..6].join(" ");
        if let Ok(dt) = DateTime::parse_from_rfc2822(&date_str) {
            return Ok(Some(dt.timestamp()));
        }
    }

    if parts.len() >= 7 {
        let date_str = format!("{} {} {}", parts[3], parts[4], parts[5]);
        if let Ok(dt) = chrono::DateTime::parse_from_str(&date_str, "%a %b %d") {
            if let Ok(year) = parts[6].parse::<i32>() {
                let with_year = dt.with_year(year);
                if let Some(dt_with_year) = with_year {
                    return Ok(Some(dt_with_year.timestamp()));
                }
            }
        }
    }

    Ok(None)
}

fn extract_subject(content: &str) -> Result<String> {
    let subject_line = content
        .lines()
        .find(|line| line.starts_with("Subject:"))
        .context("patch does not contain a 'Subject:' header")?;

    let subject_value = subject_line
        .strip_prefix("Subject:")
        .context("failed to strip 'Subject:' prefix")?
        .trim();

    Ok(cleanup_subject(subject_value))
}

fn cleanup_subject(subject: &str) -> String {
    let mut result = subject.to_string();

    loop {
        let trimmed = result.trim();

        if trimmed.starts_with("Re:") || trimmed.starts_with("re:") {
            result = trimmed[3..].trim().to_string();
            continue;
        }

        if let Some(stripped) = trimmed.strip_prefix(':') {
            result = stripped.trim().to_string();
            continue;
        }

        if trimmed.starts_with('[') {
            if let Some(end) = trimmed.find(']') {
                result = trimmed[end + 1..].trim().to_string();
                continue;
            }
        }

        break;
    }

    result
}

fn extract_commit_message_body(content: &str) -> Result<String> {
    let mut in_body = false;
    let mut body_lines: Vec<String> = Vec::new();
    let mut found_first_content = false;

    for line in content.lines() {
        if !in_body {
            if line.is_empty() {
                in_body = true;
            }
            continue;
        }

        if line.starts_with("diff --git ")
            || line.starts_with("Index: ")
            || line.starts_with("--- ")
            || line.starts_with("From ")
        {
            break;
        }

        if line.starts_with("---") && line.trim().eq("---") {
            break;
        }

        if line.starts_with("-- ") || line.starts_with("--\n") {
            break;
        }

        if !found_first_content && line.trim().is_empty() {
            continue;
        }

        found_first_content = true;
        body_lines.push(line.to_string());
    }

    while body_lines.last().is_some_and(|l| l.trim().is_empty()) {
        body_lines.pop();
    }

    Ok(body_lines.join("\n").trim().to_string())
}

pub fn extract_description_from_patch(content: &str) -> Result<String> {
    let subject = extract_subject(content)?;
    let body = extract_commit_message_body(content)?;

    if body.is_empty() {
        Ok(subject)
    } else {
        Ok(format!("{}\n\n{}", subject, body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_patch() -> String {
        "\
From 431b84edc0d2fa118d63faa3c2db9c73d630a5ae Mon Sep 17 00:00:00 2001
From: Joe Bloggs <joe.bloggs@pm.me>
Date: Thu, 1 Jan 1970 00:00:00 +0000
Subject: [PATCH] add t2.md

This is the commit message body.

It can have multiple lines.

---
 t2.md | 1 +
 1 file changed, 1 insertion(+)
 create mode 100644 t2.md

diff --git a/t2.md b/t2.md
new file mode 100644
index 0000000..a66525d
--- /dev/null
+++ b/t2.md
@@ -0,0 +1 @@
+some content1
\\ No newline at end of file
--
libgit2 1.9.1

"
        .to_string()
    }

    #[test]
    fn parse_commit_id() {
        let patch = sample_patch();
        let result = extract_commit_id_from_mbox(&patch).unwrap();
        assert_eq!(result, "431b84edc0d2fa118d63faa3c2db9c73d630a5ae");
    }

    #[test]
    fn parse_author() {
        let patch = sample_patch();
        let (name, email) = extract_author_from_from_header(&patch).unwrap();
        assert_eq!(name, "Joe Bloggs");
        assert_eq!(email, "joe.bloggs@pm.me");
    }

    #[test]
    fn parse_author_with_quoted_name() {
        let patch = "\
From abc123 Mon Sep 17 00:00:00 2001
From: \"John (nickname) Doe\" <john.doe@example.com>
Date: Thu, 1 Jan 1970 00:00:00 +0000
Subject: test

Body
";
        let (name, email) = extract_author_from_from_header(patch).unwrap();
        assert_eq!(name, "John (nickname) Doe");
        assert_eq!(email, "john.doe@example.com");
    }

    #[test]
    fn parse_author_email_only() {
        let patch = "\
From abc123 Mon Sep 17 00:00:00 2001
From: john.doe@example.com
Date: Thu, 1 Jan 1970 00:00:00 +0000
Subject: test

Body
";
        let (name, email) = extract_author_from_from_header(patch).unwrap();
        assert_eq!(name, "john.doe");
        assert_eq!(email, "john.doe@example.com");
    }

    #[test]
    fn parse_date() {
        let patch = sample_patch();
        let (timestamp, offset) = extract_date_from_header(&patch).unwrap();
        assert_eq!(timestamp, 0);
        assert_eq!(offset, 0);
    }

    #[test]
    fn parse_date_with_timezone() {
        let patch = "\
From abc123 Mon Sep 17 00:00:00 2001
From: Joe <joe@example.com>
Date: Thu, 1 Jan 1970 00:00:00 +0500
Subject: test

Body
";
        let (timestamp, offset) = extract_date_from_header(patch).unwrap();
        assert_eq!(timestamp, -18000);
        assert_eq!(offset, 300);
    }

    #[test]
    fn parse_subject() {
        let patch = sample_patch();
        let subject = extract_subject(&patch).unwrap();
        assert_eq!(subject, "add t2.md");
    }

    #[test]
    fn parse_subject_with_patch_prefix() {
        let patch = "\
From abc123 Mon Sep 17 00:00:00 2001
From: Joe <joe@example.com>
Date: Thu, 1 Jan 1970 00:00:00 +0000
Subject: [PATCH v2 3/5] fix: important bug

Body
";
        let subject = extract_subject(patch).unwrap();
        assert_eq!(subject, "fix: important bug");
    }

    #[test]
    fn parse_subject_with_re_prefix() {
        let patch = "\
From abc123 Mon Sep 17 00:00:00 2001
From: Joe <joe@example.com>
Date: Thu, 1 Jan 1970 00:00:00 +0000
Subject: Re: [PATCH] fix: important bug

Body
";
        let subject = extract_subject(patch).unwrap();
        assert_eq!(subject, "fix: important bug");
    }

    #[test]
    fn parse_body() {
        let patch = sample_patch();
        let body = extract_commit_message_body(&patch).unwrap();
        assert_eq!(
            body,
            "This is the commit message body.\n\nIt can have multiple lines."
        );
    }

    #[test]
    fn parse_body_empty() {
        let patch = "\
From abc123 Mon Sep 17 00:00:00 2001
From: Joe <joe@example.com>
Date: Thu, 1 Jan 1970 00:00:00 +0000
Subject: test

---
 file.txt | 1 +
diff --git a/file.txt b/file.txt
";
        let body = extract_commit_message_body(patch).unwrap();
        assert_eq!(body, "");
    }

    #[test]
    fn parse_full_metadata() {
        let patch = sample_patch();
        let metadata = parse_mbox_patch(&patch).unwrap();

        assert_eq!(
            metadata.commit_id,
            "431b84edc0d2fa118d63faa3c2db9c73d630a5ae"
        );
        assert_eq!(metadata.author_name, "Joe Bloggs");
        assert_eq!(metadata.author_email, "joe.bloggs@pm.me");
        assert_eq!(metadata.author_timestamp, 0);
        assert_eq!(metadata.author_offset_minutes, 0);
        assert_eq!(metadata.subject, "add t2.md");
        assert_eq!(
            metadata.body,
            "This is the commit message body.\n\nIt can have multiple lines."
        );
    }

    #[test]
    fn extract_description_combines_subject_and_body() {
        let patch = sample_patch();
        let description = extract_description_from_patch(&patch).unwrap();
        assert_eq!(
            description,
            "add t2.md\n\nThis is the commit message body.\n\nIt can have multiple lines."
        );
    }

    #[test]
    fn extract_description_subject_only() {
        let patch = "\
From abc123 Mon Sep 17 00:00:00 2001
From: Joe <joe@example.com>
Date: Thu, 1 Jan 1970 00:00:00 +0000
Subject: [PATCH] simple fix

---
 file.txt | 1 +
";
        let description = extract_description_from_patch(patch).unwrap();
        assert_eq!(description, "simple fix");
    }

    #[test]
    fn cleanup_subject_strips_patch_prefixes() {
        assert_eq!(cleanup_subject("[PATCH] test"), "test");
        assert_eq!(cleanup_subject("[PATCH v2] test"), "test");
        assert_eq!(cleanup_subject("[PATCH 1/3] test"), "test");
        assert_eq!(cleanup_subject("[PATCH v2 1/3] test"), "test");
        assert_eq!(cleanup_subject("Re: [PATCH] test"), "test");
        assert_eq!(cleanup_subject("re: test"), "test");
        assert_eq!(cleanup_subject(":test"), "test");
    }
}
