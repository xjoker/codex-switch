import contextlib
import io
import json
from pathlib import Path
import runpy
import subprocess
import sys
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).with_name("check-release-ci.py")
SHA = "1" * 40
REPO = "xjoker/codex-switch"
RUN_URL = f"https://github.com/{REPO}/actions/runs/123"
JOBS = ["Format and audit", "Test (linux)", "Test (macos)", "Test (windows)"]


def run_result(**overrides):
    return {
        "id": 123,
        "created_at": "2026-11-18T07:00:00Z",
        "run_attempt": 1,
        "event": "push",
        "status": "completed",
        "conclusion": "success",
        "head_sha": SHA,
        "head_branch": "dev",
        "head_repository": {"full_name": REPO},
        "html_url": RUN_URL,
    } | overrides


def job_result(name, **overrides):
    return {
        "id": 1000 + JOBS.index(name),
        "name": name,
        "run_attempt": 1,
        "head_sha": SHA,
        "status": "completed",
        "conclusion": "success",
    } | overrides


class ReleaseCITests(unittest.TestCase):
    def check(self, runs, jobs):
        responses = {
            f"repos/{REPO}/actions/workflows/ci.yml/runs": [{"workflow_runs": runs}],
            f"repos/{REPO}/actions/runs/123/jobs": [{"jobs": jobs}],
        }

        def gh(args, **kwargs):
            self.assertEqual(args[:2], ["gh", "api"])
            endpoint = next(arg for arg in args if arg.startswith("repos/"))
            return subprocess.CompletedProcess(args, 0, stdout=json.dumps(responses[endpoint]))

        argv = [str(SCRIPT), SHA, REPO]
        stdout, stderr = io.StringIO(), io.StringIO()
        with patch("subprocess.run", side_effect=gh), patch.object(sys, "argv", argv):
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                with self.assertRaises(SystemExit) as exit_status:
                    runpy.run_path(str(SCRIPT), run_name="__main__")
        return subprocess.CompletedProcess(argv, exit_status.exception.code, stdout.getvalue(), stderr.getvalue())

    def test_same_sha_with_all_required_jobs_can_release(self):
        jobs = [job_result(name) for name in JOBS]

        result = self.check([run_result()], jobs)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(RUN_URL, result.stdout)

    def test_other_sha_branch_or_pull_request_ci_cannot_release(self):
        result = self.check(
            [
                run_result(head_sha="2" * 40),
                run_result(head_branch="master"),
                run_result(event="pull_request"),
                run_result(head_repository={"full_name": "fork/codex-switch"}),
            ],
            [],
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(SHA, result.stderr)

    def test_pending_or_failed_branch_ci_cannot_release(self):
        for status, conclusion in (("in_progress", None), ("completed", "failure")):
            with self.subTest(status=status, conclusion=conclusion):
                result = self.check([run_result(status=status, conclusion=conclusion)], [])

                self.assertNotEqual(result.returncode, 0)
                self.assertIn(RUN_URL, result.stderr)

    def test_skipped_or_missing_host_cannot_release(self):
        jobs = [job_result(name) for name in JOBS[:-1]]
        for windows in ([], [job_result(JOBS[-1], conclusion="skipped")]):
            with self.subTest(windows=windows):
                result = self.check([run_result(event="workflow_dispatch")], jobs + windows)

                self.assertNotEqual(result.returncode, 0)
                self.assertIn(JOBS[-1], result.stderr)

    def test_successful_partial_rerun_preserves_successful_jobs(self):
        jobs = [job_result(name) for name in JOBS[:-1]]
        jobs.append(job_result(JOBS[-1], conclusion="failure"))
        jobs.append(job_result(JOBS[-1], id=2000, run_attempt=2))

        result = self.check([run_result(run_attempt=2)], jobs)

        self.assertEqual(result.returncode, 0, result.stderr)

    def test_old_success_does_not_cover_a_failed_latest_attempt(self):
        jobs = [job_result(name) for name in JOBS]
        jobs.append(job_result(JOBS[-1], id=2000, run_attempt=2, conclusion="failure"))

        result = self.check([run_result(run_attempt=2)], jobs)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(JOBS[-1], result.stderr)


if __name__ == "__main__":
    unittest.main()
