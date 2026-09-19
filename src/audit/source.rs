//! The source-side half of the audit: what the code says, not what GA4 counted.
//!
//! Every other check in `audit.rs` reads the GA4 API, which means it can only
//! ever see consequences. `double_counting` fires on a shape — a flat bounce
//! rate beside too many views per session — and then has to say "usually a
//! gtag.js snippet left in the page beside a GTM tag", because from the API
//! side the cause is a guess. Reading the project turns that sentence into a
//! file name.
//!
//! One defect is the reason this exists at all. A single-page app fires
//! `page_view` once, on load, and client-side navigation changes the URL
//! without a reload — so every route after the first goes uncounted unless the
//! app sends one itself. `docs/setup-ga4.html` has said so since before any of
//! this existed. From the data side it is nearly invisible: it looks like a
//! site where everybody lands on `/` and leaves, which reads as a content
//! problem and gets treated as one. From the source it is four lines of grep.
//!
//! These are all pure functions over text. No network, no I/O, no parser —
//! every check here is a substring question, which is the level of confidence
//! the findings are written at. A check that would need to know what the code
//! *means* rather than what it says does not belong in here.
//!
//! Nothing in here carries a [`Fix`](super::Fix). `--fix` escalates the Google
//! OAuth scope before it writes and then tells the reader that everything it
//! did is undone from the GA4 console in one click; both stop being true the
//! moment a fix edits somebody's source. So these findings name the remedy in
//! their `detail` and leave the doing to a human, which is what the eleven GA4
//! findings without fixes already do.

use super::{Finding, Grade};
use crate::lovable::{measurement_ids, wiring};

/// One file the audit was handed, already fetched.
pub(crate) struct File {
    pub path: String,
    pub text: String,
}

/// Does this file load Google Analytics at all, under any of the shapes it
/// arrives in — the gtag snippet, Tag Manager, a React wrapper, or an id read
/// out of an environment variable the way Lovable's own connector installs it.
fn tagged(file: &File) -> bool {
    !measurement_ids(&file.text).is_empty() || wiring(&file.text).is_some() || gtm(&file.text)
}

/// Google Tag Manager, which is a second way to end up with a `page_view`.
fn gtm(text: &str) -> bool {
    text.contains("googletagmanager.com/gtm.js")
        || text.contains("GTM-")
        || text.contains("dataLayer.push({'gtm.start'")
}

/// How many times this file configures a GA4 property.
///
/// The snippet `craft configure` prints has exactly one, and carries the
/// measurement id twice — once in the script `src` and once here. Counting
/// `config` calls rather than ids is what tells a correct install from a
/// doubled one.
fn config_calls(text: &str) -> usize {
    ["gtag('config'", "gtag(\"config\"", "gtag(`config`"]
        .iter()
        .map(|needle| text.matches(needle).count())
        .sum()
}

/// Does the project route on the client?
///
/// Only the routers worth naming. A false negative here costs one finding; a
/// false positive would tell somebody with a plain multi-page site to fix a
/// problem they do not have.
fn routes(text: &str) -> bool {
    [
        "react-router",
        "@tanstack/react-router",
        "@tanstack/react-start",
        "next/router",
        "next/navigation",
        "vue-router",
        "svelte-routing",
        "@remix-run",
        "wouter",
        "createBrowserRouter",
        "createRouter",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

/// Does anything here send a `page_view` of its own?
///
/// Generous on purpose. Somebody who has thought about this at all leaves one
/// of these behind, and the cost of missing one is telling a person who
/// already solved the problem that they have it.
fn sends_page_view(text: &str) -> bool {
    [
        "'page_view'",
        "\"page_view\"",
        "send_page_view",
        "ReactGA.send",
        ".pageview(",
        "pageview",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

/// Run every source check over a project's files.
pub(crate) fn examine(files: &[File]) -> Vec<Finding> {
    let mut findings = Vec::new();

    // --- tag_missing ------------------------------------------------------
    let carrying: Vec<&File> = files.iter().filter(|f| tagged(f)).collect();

    if carrying.is_empty() {
        findings.push(Finding {
            check: "tag_missing",
            grade: Grade::Critical,
            headline: "no analytics tag in the source".into(),
            detail: "nothing in this project loads Google Analytics, so nothing it does can \
                     be measured — a property with no tag pointing at it stays empty no \
                     matter how many people visit. `craft configure <domain>` creates the \
                     property and prints the snippet to paste into the page that wraps \
                     every route."
                .into(),
            evidence: Some(format!("{} files read", files.len())),
        });
        // Every other check here is a statement about a tag. Without one they
        // would all be ways of saying this again.
        return findings;
    }

    // --- tag_duplicated ---------------------------------------------------
    //
    // Three shapes of the same defect, and the evidence differs for each, so
    // the reader can tell which one they have.
    let mut ids: Vec<(&str, String)> = Vec::new(); // (path, id)
    for file in &carrying {
        for id in measurement_ids(&file.text) {
            ids.push((file.path.as_str(), id));
        }
    }

    let mut distinct: Vec<&str> = ids.iter().map(|(_, id)| id.as_str()).collect();
    distinct.sort_unstable();
    distinct.dedup();

    let doubled_config: Vec<&&File> = carrying
        .iter()
        .filter(|f| config_calls(&f.text) > 1)
        .collect();
    let with_gtm: Vec<&&File> = carrying.iter().filter(|f| gtm(&f.text)).collect();
    let with_gtag = carrying
        .iter()
        .any(|f| !measurement_ids(&f.text).is_empty() || wiring(&f.text).is_some());

    let evidence = if distinct.len() > 1 {
        Some(
            ids.iter()
                .map(|(path, id)| format!("{id} in {path}"))
                .collect::<Vec<_>>()
                .join(" · "),
        )
    } else if ids.len() > 1 {
        Some(format!(
            "{} in {}",
            distinct.first().copied().unwrap_or("the tag"),
            ids.iter()
                .map(|(p, _)| *p)
                .collect::<Vec<_>>()
                .join(" and ")
        ))
    } else if !doubled_config.is_empty() {
        Some(format!(
            "{} configures it {} times",
            doubled_config[0].path,
            config_calls(&doubled_config[0].text)
        ))
    } else if !with_gtm.is_empty() && with_gtag {
        Some(format!(
            "gtag.js and Tag Manager, in {}",
            with_gtm
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>()
                .join(" and ")
        ))
    } else {
        None
    };

    if let Some(evidence) = evidence {
        findings.push(Finding {
            check: "tag_duplicated",
            grade: Grade::Warning,
            headline: "the page is measured more than once".into(),
            detail: "a second GA4 `config` for the same id sends a second `page_view` on \
                     every load, which doubles the views and engages every session that \
                     would otherwise have bounced — so the numbers look better than the \
                     site is. Leave one, and if Tag Manager is on the site configure GA4 \
                     inside the container rather than beside it."
                .into(),
            evidence: Some(evidence),
        });
    }

    // --- spa_page_views ---------------------------------------------------
    //
    // The tag is somewhere, the app routes on the client, and nothing sends a
    // view when the route changes. That is every route after the first
    // going uncounted, and it is invisible from the GA4 side — it arrives as a
    // site where everybody lands on one page and leaves.
    let routed: Vec<&File> = files.iter().filter(|f| routes(&f.text)).collect();

    if !routed.is_empty() && !files.iter().any(|f| sends_page_view(&f.text)) {
        findings.push(Finding {
            check: "spa_page_views",
            grade: Grade::Critical,
            headline: "only the first page of each visit is counted".into(),
            detail: "the snippet sends one `page_view` when the page loads, and this app \
                     changes routes without reloading — so every page somebody reaches by \
                     clicking is missing from the reports. It reads as a site where \
                     everyone lands and leaves, which is why it is usually mistaken for a \
                     content problem. Send one yourself on every route change:\n    \
                     gtag('event', 'page_view', { page_path: location.pathname + \
                     location.search, page_location: location.href, page_title: \
                     document.title });"
                .into(),
            evidence: Some(
                routed
                    .iter()
                    .take(3)
                    .map(|f| f.path.as_str())
                    .collect::<Vec<_>>()
                    .join(" · "),
            ),
        });
    }

    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, text: &str) -> File {
        File {
            path: path.into(),
            text: text.into(),
        }
    }

    fn slugs(findings: &[Finding]) -> Vec<&'static str> {
        findings.iter().map(|f| f.check).collect()
    }

    /// The snippet `craft configure` actually prints, which every one of these
    /// checks has to read as a *correct* install.
    fn good_tag() -> String {
        crate::configure::tag_snippet("G-1A2BCD345E")
    }

    #[test]
    fn a_project_with_nothing_in_it_is_told_so_once() {
        let found = examine(&[file("index.html", "<h1>hi</h1>")]);
        assert_eq!(slugs(&found), vec!["tag_missing"]);
    }

    #[test]
    fn the_tag_craft_prints_is_not_a_double_count() {
        // It carries the measurement id twice on purpose — once in the script
        // src, once in the config call — and `configure.rs` has a test pinning
        // that. Reading it as two tags would warn every correctly tagged site.
        let found = examine(&[file("index.html", &good_tag())]);
        assert!(
            !slugs(&found).contains(&"tag_duplicated"),
            "the documented snippet read as a duplicate: {:?}",
            slugs(&found)
        );
    }

    #[test]
    fn two_configs_in_one_file_is_a_double_count() {
        let doubled = format!("{}\n{}", good_tag(), "gtag('config', 'G-1A2BCD345E');");
        let found = examine(&[file("index.html", &doubled)]);
        assert!(slugs(&found).contains(&"tag_duplicated"));
    }

    #[test]
    fn the_same_tag_in_two_files_is_a_double_count() {
        let found = examine(&[
            file("index.html", &good_tag()),
            file("src/main.tsx", &good_tag()),
        ]);
        assert!(slugs(&found).contains(&"tag_duplicated"));
    }

    #[test]
    fn two_different_properties_is_a_double_count_and_names_both() {
        let found = examine(&[
            file("index.html", &good_tag()),
            file("src/a.ts", "gtag('config', 'G-9Z8YXW765V');"),
        ]);
        let dup = found
            .iter()
            .find(|f| f.check == "tag_duplicated")
            .expect("a duplicate");
        let evidence = dup.evidence.clone().unwrap_or_default();
        assert!(evidence.contains("G-1A2BCD345E"), "{evidence}");
        assert!(evidence.contains("G-9Z8YXW765V"), "{evidence}");
    }

    #[test]
    fn gtag_beside_tag_manager_is_a_double_count() {
        let found = examine(&[
            file("index.html", &good_tag()),
            file("src/gtm.ts", "googletagmanager.com/gtm.js?id=GTM-ABC1234"),
        ]);
        assert!(slugs(&found).contains(&"tag_duplicated"));
    }

    #[test]
    fn a_routed_app_that_never_sends_a_view_is_the_finding_that_matters() {
        // The shape of the real test project: no index.html, routes under
        // src/routes, and a tag that fires exactly once.
        let found = examine(&[
            file("src/routes/__root.tsx", &good_tag()),
            file(
                "src/router.tsx",
                "import { createRouter } from '@tanstack/react-router';",
            ),
        ]);
        assert!(
            slugs(&found).contains(&"spa_page_views"),
            "got {:?}",
            slugs(&found)
        );
    }

    #[test]
    fn an_app_that_already_sends_one_is_left_alone() {
        let found = examine(&[
            file("src/routes/__root.tsx", &good_tag()),
            file(
                "src/router.tsx",
                "import { createRouter } from '@tanstack/react-router';\n\
                 gtag('event', 'page_view', { page_path: p });",
            ),
        ]);
        assert!(!slugs(&found).contains(&"spa_page_views"));
    }

    #[test]
    fn a_plain_page_with_no_router_is_not_told_to_fix_routing() {
        // A false positive here sends somebody with a static site hunting for
        // a bug they do not have.
        let found = examine(&[file("index.html", &good_tag())]);
        assert!(!slugs(&found).contains(&"spa_page_views"));
    }

    #[test]
    fn a_tag_kept_in_an_environment_variable_still_counts_as_tagged() {
        // How Lovable's own Google Analytics connector installs it: the id is
        // never in the source. Reporting "no analytics tag" would be wrong.
        let found = examine(&[file(
            "src/main.tsx",
            "const id = import.meta.env.VITE_GA_MEASUREMENT_ID;\nReactGA.initialize(id);",
        )]);
        assert!(
            !slugs(&found).contains(&"tag_missing"),
            "{:?}",
            slugs(&found)
        );
    }

    #[test]
    fn a_clean_tagged_static_site_produces_nothing() {
        assert!(examine(&[file("index.html", &good_tag())]).is_empty());
    }
}
