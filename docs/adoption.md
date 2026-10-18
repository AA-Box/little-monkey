# Adoption and evidence playbook

Little Monkey is developed in the open, but **stars, forks and release asset counts do not establish unique installations, active users or successful agent runs**. This is an operational guide for improving discoverability while keeping the evidence honest.

## Publish a real demo, not a mockup

The README links to the [first-run coding task](first-run.md), which ships with an intentionally failing test. A useful first video is one uninterrupted recording of this example on a clean machine:

1. Display the platform, **exact released app version** and selected model/runtime. Record only non-sensitive information.
2. Run the failing baseline test in a terminal (or other independent test runner).
3. Open Little Monkey, attach the demo workspace and paste the published objective.
4. Show the permissions it requests. Approve only expected actions.
5. Show the actual outcome, tool trace and `git diff`. Independently rerun the test after the run.
6. If the task fails, say so. If you retry, disclose the retry. Do not splice different attempts into one apparent uninterrupted success.
7. Record the runtime, machine, model, elapsed time, attempts and exact Git commit/release tag. Remove credentials, private paths, sessions and other identifying data before publishing.

**Only once captured:** add a screenshot or short hosted GIF/video preview to the top of `README.md`, clearly labeling it as an *actual recording from version X on platform Y*. Prefer a link to a captioned full recording plus a short preview. Do not use a mock screenshot, generated image, fabricated log or unverified benchmark.

## Which version does a claim apply to?

- The repository's default `develop` branch and [features](features.md) / [limitations](limitations.md) document development state. They can contain functionality newer than the latest installer.
- [Latest release](https://github.com/AA-Box/little-monkey/releases/latest) and its tag-specific source/docs are the source of truth for what was shipped. A new commit on `develop` does not change an existing release.
- For demo videos and public posts, use a released tag and identify features that require optional components or a configured account/model.

## Measure adoption without fabricating it

| Signal | What it measures | What it does **not** measure |
| --- | --- | --- |
| GitHub stars / forks | Public interest and derivative repositories | Installations or actual usage |
| GitHub traffic (Insights → Traffic) | Repository views and clones in GitHub's available window | Unique app users or all discovery channels |
| Public release asset download counts | Asset download requests as counted by GitHub | Unique people, installs, or real usage |
| [First-run feedback](https://github.com/AA-Box/little-monkey/issues/new/choose) | Voluntary reports of success, failure or blockers | Representative success or retention rate |
| Independently repeatable live tests | Verified outcomes for specific model/task/environment | Generic product benchmarks without a larger sample |

Run `node scripts/release-download-report.mjs` **from the repository root** to print the public GitHub download requests for the latest release, grouping installer assets by platform while excluding signature files, SBOM and extensions. It does not fetch private data, upload user information, or derive unique users. Run its deterministic checks with `node --test scripts/release-download-report.test.mjs`.

For recurring measurement, record the *date of the snapshot*, tag, downloads by actual installer asset, GitHub traffic window (if authorized), number of **distinct voluntary first-run reports**, and how many reported independent verification. Do not claim retention until a consent-based return-user measurement exists. Do not bake download totals into the README as static "user" numbers.

## Turn existing features into something people can evaluate

- Lead with the **coding task + test + diff + permission boundary**. The long-form [feature inventory](features.md) is secondary.
- Share the [reproducible task](first-run.md) in relevant developer discussions, with a disclosed affiliation. Invite honest reports, not stars.
- After collecting real recordings, link them from the README and website. Publish the exact release and model used.
- Keep release notes explicit about newly shipped capabilities. Give anyone using a stable installer a matching documentation tag.
- Prefer reproducible technical comparisons with disclosed environment and baselines. A fabricated comparative score is worse than no score.

### Short post, only after a real demo is recorded

> I work on Little Monkey, an MIT-licensed local AI coding agent. I recorded a real run fixing a deliberately broken JavaScript test, including the permissions, file diff and independently rerun tests. The exact app release, model and environment are in the recording. Source and the reproducible example: https://github.com/AA-Box/little-monkey. Feedback about the first-run experience is welcome.

Do not use the passage about recording until a real recording exists. Public distribution requires publishing on the relevant channels; updating GitHub documentation does **not** create independent reviews, endorsements, installations, or an external audience.
