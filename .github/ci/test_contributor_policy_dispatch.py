"""Exercise the actual no-checkout policy scripts without network or status writes."""

import copy
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest


WORKFLOW = Path(__file__).resolve().parents[1] / "workflows/contributor-policy.yml"
REPOSITORY = "artificial-intelligence-edge/PineWASM"
REPOSITORY_ID = 1470159731
ROUTE = "contributor-policy"
HEAD = "a" * 40


def inline_script(step_name):
    lines = WORKFLOW.read_text().splitlines()
    start = lines.index("      - name: " + step_name)
    begin = next(i for i in range(start, len(lines)) if lines[i] == "        run: |")
    first = next(line for line in lines[begin + 1 :] if line.strip())
    indentation = len(first) - len(first.lstrip())
    if indentation <= 8:
        raise AssertionError("inline script is empty or malformed")
    body = []
    for line in lines[begin + 1 :]:
        if line and not line.startswith(" " * indentation):
            break
        body.append(line[indentation:] if line else "")
    if not body:
        raise AssertionError("inline script cannot be empty")
    return "\n".join(body) + "\n"


def pull_request():
    repository = {"id": REPOSITORY_ID, "full_name": REPOSITORY}
    return {
        "number": 24,
        "state": "open",
        "user": {"login": "jdehorty"},
        "head": {"sha": HEAD, "ref": ROUTE, "repo": copy.deepcopy(repository)},
        "base": {"ref": "main", "repo": copy.deepcopy(repository)},
    }


class ContributorDispatchControls(unittest.TestCase):
    def setUp(self):
        self.assertIsNotNone(shutil.which("jq"), "the policy runner requires jq")
        self.temporary = tempfile.TemporaryDirectory(
            prefix="contributor-policy-test-"
        )
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        binaries = self.root / "bin"
        binaries.mkdir()
        mock = binaries / "curl"
        mock.write_text(
            "#!"
            + sys.executable
            + "\n"
            + textwrap.dedent(
                """\
                import json, os, sys
                from pathlib import Path
                args = sys.argv[1:]
                method = args[args.index('--request')+1] if '--request' in args else 'GET'
                url = args[-1]
                record = {'method':method,'url':url}
                if '--data' in args:
                    record['payload'] = json.loads(args[args.index('--data')+1])
                with Path(os.environ['MOCK_CALLS']).open('a') as f:
                    f.write(json.dumps(record)+'\\n')
                if method == 'GET':
                    if os.environ.get('MOCK_GET_FAIL') == '1':
                        print('curl: simulated API failure',file=sys.stderr);sys.exit(22)
                    state = Path(os.environ['MOCK_STATE'])
                    index = int(state.read_text()) if state.exists() else 0
                    state.write_text(str(index+1))
                    responses = json.loads(Path(os.environ['MOCK_RESPONSES']).read_text())
                    response = responses[min(index,len(responses)-1)]
                    print(response if isinstance(response,str) else json.dumps(response))
                elif method == 'POST':
                    if os.environ.get('MOCK_POST_FAIL') == '1':sys.exit(22)
                    print('{}')
                else:
                    sys.exit(99)
                """
            )
        )
        mock.chmod(0o700)
        self.env = {
            **os.environ,
            "PATH": str(binaries) + os.pathsep + os.environ["PATH"],
            "POLICY_RUNNER_OS": "Linux",
            "POLICY_RUNNER_ARCH": "X64",
            "POLICY_RUNNER_ENVIRONMENT": "github-hosted",
            "GITHUB_REPOSITORY": REPOSITORY,
            "GITHUB_REPOSITORY_ID": str(REPOSITORY_ID),
            "GITHUB_API_URL": "https://api.github.com",
            "GITHUB_SERVER_URL": "https://github.com",
            "GITHUB_EVENT_NAME": "workflow_dispatch",
            "GITHUB_ACTOR": "jdehorty",
            "GITHUB_TRIGGERING_ACTOR": "jdehorty",
            "GITHUB_REF": "refs/heads/" + ROUTE,
            "GITHUB_SHA": HEAD,
            "GITHUB_RUN_ID": "12345",
            "PR_AUTHOR": "",
            "PR_HEAD_REPOSITORY_ID": "",
            "BASE_REPOSITORY_ID": str(REPOSITORY_ID),
            "CANDIDATE_SHA": "",
            "DISPATCH_PR_NUMBER": "24",
            "GH_TOKEN": "fixture-token-must-not-print",
            "MOCK_CALLS": str(self.root / "calls.jsonl"),
            "MOCK_STATE": str(self.root / "get-count"),
            "MOCK_RESPONSES": str(self.root / "responses.json"),
        }
        self.set_responses([pull_request()])

    def set_responses(self, responses):
        (self.root / "responses.json").write_text(json.dumps(responses))

    def execute(self, **changes):
        env = {**self.env, **changes}
        runner_env = {key: value for key, value in env.items() if key != "GH_TOKEN"}
        command = ["bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c"]
        first = subprocess.run(
            command + [inline_script("Verify fixed policy execution context")],
            env=runner_env,
            text=True,
            capture_output=True,
        )
        if first.returncode:
            return first
        return subprocess.run(
            command + [inline_script("Enforce pending outside-contribution policy")],
            env=env,
            text=True,
            capture_output=True,
        )

    def calls(self):
        path = self.root / "calls.jsonl"
        return (
            [json.loads(line) for line in path.read_text().splitlines()]
            if path.exists()
            else []
        )

    def posts(self):
        return [row for row in self.calls() if row["method"] == "POST"]

    def test_exact_owned_route_and_no_checkout_or_extra_privileges(self):
        text = WORKFLOW.read_text()
        self.assertIn(
            'runs-on: "ubuntu-24.04"',
            text,
        )
        self.assertIn("pull_request_target:", text)
        self.assertIn("workflow_dispatch:", text)
        # Reading pull request metadata requires pull-requests:read.
        self.assertIn("contents: read", text)
        self.assertIn("pull-requests: read", text)
        self.assertIn("statuses: write", text)
        self.assertNotIn("contents: write", text)
        self.assertNotRegex(text, r"(?m)^\s*uses:")
        self.assertNotIn("github.event.pull_request.title", text)
        self.assertNotIn("github.event.pull_request.body", text)
        self.assertNotIn("inputs.runner", text)
        self.assertNotIn("inputs.sha", text)
        self.assertNotIn("inputs.state", text)

    def test_manual_positive_publishes_only_the_current_exact_head(self):
        result = self.execute()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            [row["method"] for row in self.calls()], ["GET", "GET", "POST", "GET"]
        )
        self.assertEqual(len(self.posts()), 1)
        post = self.posts()[0]
        self.assertEqual(
            post["url"],
            "https://api.github.com/repos/" + REPOSITORY + "/statuses/" + HEAD,
        )
        self.assertEqual(post["payload"]["state"], "success")
        self.assertEqual(post["payload"]["context"], "Contributor acceptance policy")
        self.assertEqual(
            post["payload"]["target_url"],
            "https://github.com/" + REPOSITORY + "/actions/runs/12345",
        )
        self.assertNotIn(self.env["GH_TOKEN"], result.stdout + result.stderr)

    def test_native_rule_accepts_only_maintainer_same_repository(self):
        result = self.execute(
            GITHUB_EVENT_NAME="pull_request_target",
            PR_AUTHOR="jdehorty",
            PR_HEAD_REPOSITORY_ID=str(REPOSITORY_ID),
            CANDIDATE_SHA=HEAD,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([row["method"] for row in self.calls()], ["POST"])
        self.assertEqual(self.posts()[0]["payload"]["state"], "success")

    def test_native_outsider_still_writes_failure(self):
        result = self.execute(
            GITHUB_EVENT_NAME="pull_request_target",
            PR_AUTHOR="outside-user",
            PR_HEAD_REPOSITORY_ID=str(REPOSITORY_ID),
            CANDIDATE_SHA=HEAD,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.posts()[0]["payload"]["state"], "failure")

    def test_native_fork_still_writes_failure(self):
        result = self.execute(
            GITHUB_EVENT_NAME="pull_request_target",
            PR_AUTHOR="jdehorty",
            PR_HEAD_REPOSITORY_ID="999",
            CANDIDATE_SHA=HEAD,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.posts()[0]["payload"]["state"], "failure")

    def test_manual_api_identity_and_state_fail_closed(self):
        mutations = [
            lambda pr: pr["user"].update(login="outside-user"),
            lambda pr: pr["head"]["repo"].update(id=999),
            lambda pr: pr["head"]["repo"].update(full_name="other/repo"),
            lambda pr: pr["base"]["repo"].update(id=999),
            lambda pr: pr["base"]["repo"].update(full_name="other/repo"),
            lambda pr: pr["base"].update(ref="other"),
            lambda pr: pr["head"].update(ref="other"),
            lambda pr: pr["head"].update(sha="b" * 40),
            lambda pr: pr.update(state="closed"),
            lambda pr: pr.update(number=25),
            lambda pr: pr.pop("head"),
        ]
        for mutate in mutations:
            with self.subTest(mutation=mutate):
                pr = pull_request()
                mutate(pr)
                self.set_responses([pr])
                result = self.execute()
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.posts(), [])

    def test_stale_head_before_write_creates_no_status(self):
        changed = pull_request()
        changed["head"]["sha"] = "b" * 40
        self.set_responses([pull_request(), changed])
        self.assertNotEqual(self.execute().returncode, 0)
        self.assertEqual(self.posts(), [])

    def test_head_change_after_write_fails_and_never_targets_new_head(self):
        changed = pull_request()
        changed["head"]["sha"] = "b" * 40
        self.set_responses([pull_request(), pull_request(), changed])
        self.assertNotEqual(self.execute().returncode, 0)
        self.assertEqual(len(self.posts()), 1)
        self.assertTrue(self.posts()[0]["url"].endswith("/statuses/" + HEAD))

    def test_wrong_actor_ref_runner_or_repository_never_uses_credentials(self):
        cases = [
            {"GITHUB_ACTOR": "other"},
            {"GITHUB_TRIGGERING_ACTOR": "other"},
            {"GITHUB_REF": "refs/heads/main"},
            {"GITHUB_REF": "refs/tags/route"},
            {"GITHUB_REPOSITORY": "other/repo"},
            {"GITHUB_REPOSITORY_ID": "999"},
            {"POLICY_RUNNER_OS": "Windows"},
            {"POLICY_RUNNER_ARCH": "ARM64"},
            {"POLICY_RUNNER_ENVIRONMENT": "self-hosted"},
            {"GITHUB_API_URL": "https://other.invalid"},
        ]
        for changes in cases:
            with self.subTest(changes=changes):
                self.assertNotEqual(self.execute(**changes).returncode, 0)
                self.assertEqual(self.calls(), [])

    def test_bad_dispatch_inputs_and_api_errors_never_publish(self):
        for number in ["", "0", "-1", "1.0", " 24", "24/other", "1;false", "9" * 11]:
            with self.subTest(number=number):
                self.assertNotEqual(
                    self.execute(DISPATCH_PR_NUMBER=number).returncode, 0
                )
                self.assertEqual(self.calls(), [])
        self.assertNotEqual(self.execute(MOCK_GET_FAIL="1").returncode, 0)
        self.assertEqual(self.posts(), [])

    def test_missing_or_malformed_api_json_never_publish(self):
        for response in [{}, None, "invalid-json"]:
            with self.subTest(response=response):
                self.set_responses([response])
                self.assertNotEqual(self.execute().returncode, 0)
                self.assertEqual(self.posts(), [])

    def test_post_failure_fails_the_actual_policy_run(self):
        self.assertNotEqual(self.execute(MOCK_POST_FAIL="1").returncode, 0)

    def test_api_title_and_body_are_never_shell_code_or_log_output(self):
        pr = pull_request()
        marker = self.root / "must-not-exist"
        pr["title"] = "$(touch " + str(marker) + ")"
        pr["body"] = "private-body-must-not-print; `touch " + str(marker) + "`"
        self.set_responses([pr])
        result = self.execute()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(marker.exists())
        self.assertNotIn(pr["body"], result.stdout + result.stderr)
        self.assertNotIn(pr["title"], result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
