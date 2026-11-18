#!/usr/bin/env python3
"""Require the latest dev CI run for a release commit to pass."""

import argparse
from datetime import datetime
import json
import re
import subprocess
import sys


REQUIRED_JOBS = ("Format and audit", "Test (linux)", "Test (macos)", "Test (windows)")


def api_rows(endpoint, key, *fields):
    response = subprocess.run(
        ["gh", "api", "--method", "GET", "--paginate", "--slurp", endpoint, *fields],
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    )
    pages = json.loads(response.stdout)
    if not isinstance(pages, list):
        raise ValueError(f"invalid paginated GitHub response for {endpoint}")
    rows = []
    for page in pages:
        if not isinstance(page, dict) or not isinstance(page.get(key), list):
            raise ValueError(f"invalid {key} in GitHub response for {endpoint}")
        rows.extend(page[key])
    if any(not isinstance(row, dict) for row in rows):
        raise ValueError(f"invalid {key} entry in GitHub response for {endpoint}")
    return rows


def positive_integer(row, field):
    value = row[field]
    if type(value) is not int or value < 1:
        raise ValueError(f"invalid {field} in GitHub response")
    return value


def run_order(run):
    created = datetime.fromisoformat(run["created_at"].replace("Z", "+00:00"))
    if created.tzinfo is None:
        raise ValueError("workflow run creation time is missing a UTC offset")
    return created, positive_integer(run, "id")


def passed(row):
    return row.get("status") == "completed" and row.get("conclusion") == "success"


def check_release(sha, repo):
    actions = f"repos/{repo}/actions"
    runs = api_rows(
        f"{actions}/workflows/ci.yml/runs",
        "workflow_runs",
        "-f", f"head_sha={sha}",
        "-f", "branch=dev",
        "-F", "per_page=100",
    )
    candidates = (
        run
        for run in runs
        if run["head_sha"] == sha
        and run["head_branch"] == "dev"
        and run["event"] in ("push", "workflow_dispatch")
        and run["head_repository"]["full_name"].lower() == repo.lower()
    )
    run = max(candidates, key=run_order, default=None)
    if run is None:
        raise ValueError(f"no dev CI run for {sha} in {repo}")
    run_id = positive_integer(run, "id")
    attempt = positive_integer(run, "run_attempt")
    url = f"https://github.com/{repo}/actions/runs/{run_id}"
    if not passed(run):
        raise ValueError(
            f"latest dev CI for {sha} is {run.get('status')}/{run.get('conclusion')}: {url}"
        )

    jobs = api_rows(
        f"{actions}/runs/{run_id}/jobs", "jobs", "-f", "filter=all", "-F", "per_page=100"
    )
    for name in REQUIRED_JOBS:
        matching = [job for job in jobs if job["name"] == name]
        job = max(
            matching,
            key=lambda row: (positive_integer(row, "run_attempt"), positive_integer(row, "id")),
            default=None,
        )
        if job is None or job["head_sha"] != sha or not passed(job):
            raise ValueError(f"{name} did not pass for {sha}: {url}")
        if job["run_attempt"] > attempt:
            raise ValueError(f"a newer CI attempt started while checking {sha}: {url}")
    print(f"Verified dev CI for {sha} (latest attempt {attempt}): {url}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("sha", help="full commit SHA to release")
    parser.add_argument("repo", help="GitHub repository as OWNER/REPO")
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.sha):
        parser.error("sha must be a full lowercase Git commit SHA")
    repo_name = args.repo.rsplit("/", 1)[-1]
    if not re.fullmatch(r"[A-Za-z0-9-]+/[A-Za-z0-9_.-]+", args.repo) or repo_name in (".", ".."):
        parser.error("repo must be a GitHub OWNER/REPO")
    try:
        check_release(args.sha, args.repo)
    except (
        AttributeError, KeyError, TypeError, ValueError, OSError, subprocess.CalledProcessError
    ) as exc:
        print(f"Release CI check failed for {args.sha}: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
