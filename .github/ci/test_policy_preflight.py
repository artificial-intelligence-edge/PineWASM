"""Expensive CI jobs must wait for preflight checks and fail closed."""

import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest


WORKFLOW = Path(__file__).resolve().parents[1] / 'workflows' / 'ci.yml'
PREFLIGHT = {'plan', 'dco', 'workflow-policy', 'secret-scan'}
EXPENSIVE = {
    'dependency-policy', 'rust', 'adversarial', 'msrv',
    'aot-runtime-reproducibility', 'runtime-safety',
}


def block_mapping(source, indent):
    """Read the pinned block style; unsupported mapping syntax fails closed."""
    pattern = re.compile(
        r'^' + ' ' * indent
        + r'''(?P<quote>['"]?)(?P<key>[A-Za-z_][A-Za-z0-9_-]*)(?P=quote):[ \t]*(?P<value>.*)$'''
    )
    headers = []
    lines = source.splitlines(keepends=True)
    keys = set()
    for index, line in enumerate(lines):
        if not line.strip() or line.lstrip().startswith('#'):
            continue
        depth = len(line) - len(line.lstrip(' '))
        if depth == indent:
            header = pattern.fullmatch(line.rstrip('\r\n'))
            if header is None or header['key'] in keys:
                raise AssertionError('unsupported or duplicate workflow mapping: ' + line.strip())
            keys.add(header['key'])
            headers.append((index, header))
        elif depth < indent or not headers:
            raise AssertionError('unsupported workflow mapping indentation: ' + line.strip())
    if not headers:
        raise AssertionError('missing workflow block mapping')
    return {
        header['key']: (header['value'], ''.join(lines[index + 1:
                                       headers[number + 1][0] if number + 1 < len(headers)
                                       else len(lines)]))
        for number, (index, header) in enumerate(headers)
    }


def jobs(source):
    """Inspect every job ID, including quoted, uppercase and underscore IDs."""
    markers = list(re.finditer(r'^jobs:[ \t]*(?:#.*)?$', source, re.MULTILINE))
    if len(markers) != 1:
        raise AssertionError('expected one block-style jobs mapping')
    body = source[markers[0].end():]
    following_root = re.search(r'^[^\s#]', body, re.MULTILINE)
    if following_root:
        body = body[:following_root.start()]
    result = {}
    for name, (value, job) in block_mapping(body, 2).items():
        if value.strip() and not value.lstrip().startswith('#'):
            raise AssertionError('expected a block-style job: ' + name)
        result[name] = job
    return result


def job_fields(source):
    return {key: value for key, (value, _) in block_mapping(source, 4).items()}


def dependencies(job):
    inline = re.search(r'^    needs: \[([^\]\n]*)\]\s*$', job, re.MULTILINE)
    if inline:
        return {name.strip() for name in inline.group(1).split(',')}
    block = re.search(r'^    needs:\s*\n((?:      - [a-z0-9-]+\s*\n)+)', job, re.MULTILINE)
    if block:
        return set(re.findall(r'      - ([a-z0-9-]+)', block.group(1)))
    raise AssertionError('missing or unsupported needs syntax')


def named_steps(job):
    return re.findall(r'^      - name:.*?(?=^      - |\Z)', job, re.MULTILINE | re.DOTALL)


def workflow_paths(directory):
    """Include both file extensions recognized by GitHub Actions."""
    return sorted(path for path in directory.iterdir()
                  if path.is_file() and path.suffix in {'.yml', '.yaml'})


class JobMappingTests(unittest.TestCase):
    def test_every_valid_job_id_is_inspected(self):
        source = 'jobs:\n'
        names = ['build', '_build', 'Build', 'Build_2-x', '"Quoted"', "'_Quoted'"]
        for name in names:
            source += f'  {name}:\n    runs-on: ubuntu-24.04\n'
        self.assertEqual(set(jobs(source)), {name.strip('\'"') for name in names})

    def test_only_the_jobs_mapping_is_inspected(self):
        source = 'on:\n  push:\njobs:\n  Build:\n    runs-on: ubuntu-24.04\npermissions:\n  contents: read\n'
        self.assertEqual(set(jobs(source)), {'Build'})

    def test_unsupported_job_mappings_fail_closed(self):
        for source in [
            'jobs: {Build: {runs-on: ubuntu-24.04}}\n',
            'jobs:\n  Build: {runs-on: ubuntu-24.04}\n',
            'jobs:\n  Build: *unguarded\n',
            'jobs:\n    Build:\n      runs-on: ubuntu-24.04\n',
            'jobs:\n  1build:\n    runs-on: ubuntu-24.04\n',
            'jobs:\n  Build:\n    runs-on: ubuntu-24.04\n  Build:\n    runs-on: ubuntu-24.04\n',
        ]:
            with self.subTest(source=source), self.assertRaises(AssertionError):
                jobs(source)

    def test_quoted_job_fields_are_inspected(self):
        self.assertEqual(
            job_fields('    "runs-on": ubuntu-24.04\n    \'if\': true\n'),
            {'runs-on': 'ubuntu-24.04', 'if': 'true'},
        )

    def test_unsupported_job_field_mappings_fail_closed(self):
        for source in ['      runs-on: ubuntu-24.04\n', '    <<: *unguarded\n']:
            with self.subTest(source=source), self.assertRaises(AssertionError):
                job_fields(source)


class PolicyPreflightTests(unittest.TestCase):
    def setUp(self):
        self.workflow = jobs(WORKFLOW.read_text())

    def test_expensive_jobs_wait_for_all_preflight_checks(self):
        for name in sorted(EXPENSIVE):
            with self.subTest(job=name):
                job = self.workflow[name]
                self.assertTrue(PREFLIGHT <= dependencies(job))
                # Without a status function, GitHub's implicit success() stops
                # failed, cancelled and skipped dependencies from launching work.
                condition = re.search(r'^    if: (.*)$', job, re.MULTILINE)
                self.assertIsNotNone(condition)
                self.assertNotRegex(condition.group(1), r'\b(?:always|cancelled|failure|success)\s*\(')

    def test_preflight_checks_do_not_wait_for_expensive_validation(self):
        for name in sorted(PREFLIGHT - {'plan'}):
            with self.subTest(job=name):
                self.assertEqual(dependencies(self.workflow[name]), {'plan'})

    def test_result_still_observes_every_policy_and_validation_lane(self):
        result = self.workflow['result']
        self.assertTrue(PREFLIGHT | EXPENSIVE | {'guard', 'wasm'} <= dependencies(result))
        self.assertRegex(result, re.compile(r'^    if: .*always\(\)', re.MULTILINE))
        self.assertIn('cargo run --locked -p pinewasm_xtask -- ci-verify', result)

    def test_arm_packaging_waits_for_the_complete_native_matrix(self):
        self.assertIn('rust', dependencies(self.workflow['wasm']))
        self.assertIn('platform: [linux-x64, macos-arm64]', self.workflow['rust'])

    def test_main_pushes_and_dispatched_runs_check_every_commit_since_the_last_pass(self):
        source = WORKFLOW.read_text()
        self.assertIn("CI_DISABLE_REUSE: ${{ inputs.full || 'false' }}", source)
        triggers = block_mapping(block_mapping(source, 0)['on'][1], 2)
        self.assertEqual(set(triggers), {'push', 'workflow_dispatch'})
        self.assertEqual(block_mapping(triggers['push'][1], 4)['branches'][0], '[main]')
        self.assertNotIn('github.event.pull_request', source)
        self.assertIn("CI_FORCE_FULL: ${{ github.event_name == 'push' || inputs.full || 'false' }}", source)
        plan = self.workflow['plan']
        self.assertIn('range_base: ${{ steps.range.outputs.base }}', plan)
        [resolver] = [step for step in named_steps(plan) if '\n        id: range\n' in step]
        self.assertIn(
            "if: github.event_name == 'push' || github.event_name == 'workflow_dispatch'",
            resolver,
        )
        self.assertIn('GH_TOKEN: ${{ github.token }}', resolver)
        self.assertIn(
            'pinewasm_xtask -- ci-range-base --github-output "$GITHUB_OUTPUT"', resolver
        )
        base = 'BASE_SHA: ${{ needs.plan.outputs.range_base }}'
        for name, check in [('guard', 'check-public-boundary --git-range'),
                            ('secret-scan', 'check-secrets --git-range'),
                            ('dco', 'check-dco "${BASE_SHA}"')]:
            with self.subTest(job=name):
                [step] = [step for step in named_steps(self.workflow[name])
                          if 'BASE_SHA:' in step]
                self.assertIn(base, step)
                # A missing base fails before any range check runs.
                self.assertIn('[[ -z "${BASE_SHA}" ]]', step)
                self.assertLess(step.index('[[ -z "${BASE_SHA}" ]]'), step.index(check))
        self.assertIn(
            'check-compiler-version --first-parent "${BASE_SHA}" "${HEAD_SHA}"',
            self.workflow['guard'],
        )
        # No range check may fall back to an empty range at the run's own commit.
        self.assertNotRegex(WORKFLOW.read_text(), r'BASE_SHA: .*\|\| github\.sha')


# Fixed GitHub-hosted images, reviewed with workflows and platform support.
HOSTED_RUNNERS = {
    'linux-x64': 'ubuntu-24.04',
    'linux-arm64': 'ubuntu-24.04-arm',
    'macos-arm64': 'macos-15',
}
REPOSITORY_ID = '1470159731'
REPOSITORY = 'artificial-intelligence-edge/PineWASM'


def strict_runner_json(value):
    def unique_pairs(pairs):
        result = {}
        for key, item in pairs:
            if key in result:
                raise AssertionError('duplicate runner mapping key: ' + key)
            result[key] = item
        return result
    try:
        return json.loads(value, object_pairs_hook=unique_pairs)
    except (ValueError, TypeError) as error:
        raise AssertionError('runner route must be literal JSON') from error


def canonical_runner(value):
    if not isinstance(value, str) or value not in HOSTED_RUNNERS.values():
        raise AssertionError('runner must be an approved fixed GitHub-hosted image')
    return value


def runner_selections(value):
    """Inspect every literal selection, including dormant matrix entries."""
    value = value.strip()
    if not value.startswith('${{'):
        return {'static': canonical_runner(value.strip(chr(34)))}
    table = re.fullmatch(
        r"\$\{\{ fromJSON\('(\{[^']*\})'\)\[matrix\.platform\] \}\}", value)
    if table is None:
        raise AssertionError('unsupported or configurable runs-on form: ' + value)
    mapping = strict_runner_json(table.group(1))
    if not isinstance(mapping, dict) or not mapping:
        raise AssertionError('runner table must be a nonempty literal platform mapping')
    return {name: canonical_runner(route) for name, route in mapping.items()}


class RunnerTrustTests(unittest.TestCase):
    """Allow only approved hosted runner routes and this repository's identity guards."""

    def test_workflow_discovery_includes_both_yaml_extensions(self):
        with tempfile.TemporaryDirectory(prefix='runner-trust-workflows-') as directory:
            root = Path(directory)
            for name in ['ci.yml', 'untrusted.yaml', 'notes.txt']:
                (root / name).write_text('')
            (root / 'nested.yml').mkdir()
            self.assertEqual(
                [path.name for path in workflow_paths(root)],
                ['ci.yml', 'untrusted.yaml'],
            )

    def assert_owned_routes(self, source):
        fields = [job_fields(job) for job in jobs(source).values()]
        runners = [job['runs-on'] for job in fields if 'runs-on' in job]
        self.assertTrue(runners, 'a runnable workflow needs an explicit runner')
        allowed = {canonical_runner(route) for route in HOSTED_RUNNERS.values()}
        for runner in runners:
            self.assertTrue(set(runner_selections(runner).values()) <= allowed, runner)

    def test_every_workflow_uses_only_exact_reviewed_owned_routes(self):
        for path in workflow_paths(WORKFLOW.parent):
            with self.subTest(workflow=path.name):
                self.assert_owned_routes(path.read_text())

    def test_each_maintained_lane_keeps_its_architecture_route(self):
        for path in workflow_paths(WORKFLOW.parent):
            for name, job in jobs(path.read_text()).items():
                fields = job_fields(job)
                if 'runs-on' not in fields:
                    continue
                expected = {'static': canonical_runner(HOSTED_RUNNERS['linux-x64'])}
                if path.name == 'ci.yml' and name == 'rust':
                    expected = {platform: canonical_runner(HOSTED_RUNNERS[platform])
                                for platform in ('linux-x64', 'macos-arm64')}
                elif path.name == 'ci.yml' and name == 'wasm':
                    expected = {'static': canonical_runner(HOSTED_RUNNERS['linux-arm64'])}
                with self.subTest(workflow=path.name, job=name):
                    self.assertEqual(runner_selections(fields['runs-on']), expected)

    def test_a_dormant_unapproved_matrix_entry_is_rejected(self):
        for dormant in ['windows-2025', ['self-hosted', 'Windows', 'X64'],
                        {'group': 'untrusted', 'labels': ['self-hosted', 'Linux', 'X64']}]:
            mapping = {'linux-x64': HOSTED_RUNNERS['linux-x64'], 'dormant': dormant}
            runner = "${{ fromJSON('" + json.dumps(mapping) + "')[matrix.platform] }}"
            with self.subTest(dormant=dormant), self.assertRaises(AssertionError):
                self.assert_owned_routes(f'jobs:\n  Build:\n    runs-on: {runner}\n')

    def test_generic_hosted_unpinned_and_configurable_routes_are_rejected(self):
        for runner in [
            'self-hosted', '[self-hosted, Linux, X64]', '[ubuntu-24.04, self-hosted]',
            'windows-2025',
            'ubuntu-latest', 'macos-latest', '${{ vars.RUNNER }}',
            "${{ inputs.runner || 'ubuntu-24.04' }}",
            '${{ fromJSON(vars.RUNNERS)[matrix.platform] }}', '${{ matrix.runner }}', '',
            '{"group":"unapproved-group","labels":["self-hosted","Linux","X64"]}',
            '{"group":"Default","labels":["self-hosted","Linux","X64"]}',
            '{"group":"unapproved-group","labels":["self-hosted","Linux","X64","other-ci","other-linux","other-host"]}',
            '{"group":"unapproved-group","labels":["self-hosted"],"fallback":"ubuntu-24.04"}',
            '{"group":"unapproved-group","group":"untrusted","labels":["self-hosted"]}',
        ]:
            source = f'jobs:\n  Build:\n    runs-on: {runner}\n'
            with self.subTest(runner=runner), self.assertRaises(AssertionError):
                self.assert_owned_routes(source)

    def test_custom_runner_configuration_is_absent(self):
        self.assertEqual((WORKFLOW.parent.parent / 'actionlint.yaml').read_text().strip(), '{}')

    def test_windows_request_fails_explicitly_without_a_fallback_route(self):
        source = WORKFLOW.read_text()
        plan = jobs(source)['plan']
        self.assertIn("if: vars.ENABLE_WINDOWS_CI == 'true'", plan)
        self.assertIn('no approved Windows validation lane is configured.', plan)
        self.assertRegex(plan, re.compile(r'^          exit 1$', re.MULTILINE))
        self.assertIn('platform: [linux-x64, macos-arm64]', jobs(source)['rust'])
        self.assertNotIn('windows-2025', source)

    def test_owned_route_never_runs_the_hosted_image_deletion(self):
        source = WORKFLOW.read_text()
        self.assertNotIn('Reclaim hosted-runner disk space', source)
        self.assertNotIn('/usr/local/lib/android', source)
        self.assertNotIn('/opt/hostedtoolcache/CodeQL', source)
        self.assertIn('Report native workspace disk', source)
        self.assertIn('native_cache.py prepare', source)

    def test_python_prerequisite_is_admitted_before_selected_code_and_heavy_setup(self):
        for filename in ('ci.yml', 'runtime-safety.yml'):
            for name, job in jobs((WORKFLOW.parent / filename).read_text()).items():
                if 'runs-on' not in job_fields(job):
                    continue
                with self.subTest(workflow=filename, job=name):
                    prerequisite = job.index('if sys.version_info < (3, 11):')
                    checkout = job.index('uses: actions/checkout@')
                    self.assertLess(prerequisite, checkout)
                    self.assertIn('import tomllib', job[:checkout])
                    self.assertIn('type -P python3', job[:checkout])
                    self.assertIn('"$python_path" -I -', job[:checkout])
                    self.assertIn('no fallback', job[:checkout])

    def test_owned_route_executes_selected_gates_instead_of_old_server_results(self):
        scopes = set()
        for name, job in jobs(WORKFLOW.read_text()).items():
            steps = re.findall(r'^      - name:.*?(?=^      - |\Z)', job, re.MULTILINE | re.DOTALL)
            for step in steps:
                if 'ci-reuse --lane' not in step:
                    continue
                scopes.add(name)
                self.assertIn('CI_DISABLE_REUSE: "true"', step)
                self.assertNotIn('GH_TOKEN:', step)
                self.assertNotIn('inputs.full', step)
        self.assertEqual(
            scopes, {'rust', 'adversarial', 'msrv', 'aot-runtime-reproducibility', 'wasm'}
        )

    def test_no_workflow_saves_to_github_cache_storage(self):
        rust_caches = 0
        for path in workflow_paths(WORKFLOW.parent):
            source = path.read_text()
            self.assertNotIn('uses: actions/cache@', source)
            self.assertNotIn('uses: actions/cache/save@', source)
            for job in jobs(source).values():
                steps = re.findall(r'^      - .*?(?=^      - |\Z)', job,
                                   re.MULTILINE | re.DOTALL)
                for step in steps:
                    if 'uses: Swatinem/rust-cache@' in step:
                        rust_caches += 1
                        self.assertIn('save-if: "false"', step)
                    if 'uses: actions/setup-node@' in step:
                        self.assertIn('package-manager-cache: false', step)
        self.assertEqual(rust_caches, 14)

    def test_raw_owned_records_are_retained_without_log_echo_or_storage_admission(self):
        source = WORKFLOW.read_text()
        self.assertEqual(source.count('name: Record validation inputs'), 5)
        self.assertEqual(source.count('--kind ci-record --path'), 5)
        self.assertEqual(source.count('> "$RUNNER_TEMP/ci-record-stdout.txt"'), 5)
        self.assertNotIn('cat "$RUNNER_TEMP/ci-record-stdout.txt"', source)
        self.assertIn('Retain verified Wasm package as a workflow artifact', source)

    def test_removed_packaging_files_stay_absent(self):
        for relative in ('workflows/release-package.yml', 'release-policy.json',
                         'ci/release_package.py', 'ci/test_release_package.py'):
            self.assertFalse((WORKFLOW.parent.parent / relative).exists(), relative)

    def test_dependency_update_pr_configuration_is_absent(self):
        for filename in ('dependabot.yml', 'dependabot.yaml'):
            self.assertFalse((WORKFLOW.parent.parent / filename).exists(), filename)

    def assert_repository_identity_guards(self, source, contributor_policy=False, main_only=False, publisher=False):
        for name, job in jobs(source).items():
            fields = job_fields(job)
            if 'runs-on' not in fields:
                # Called jobs are inspected in the repository's own workflows.
                self.assertRegex(fields.get('uses', ''), r'^\./\.github/workflows/[^/]+\.ya?ml$')
                continue
            condition = fields.get('if', '')
            self.assertNotIn('#', condition, 'guard checks require an uncommented expression')
            self.assertIn(f"github.repository_id == '{REPOSITORY_ID}'", condition, name)
            self.assertIn(f"github.repository == '{REPOSITORY}'", condition, name)
            if main_only:
                self.assertIn("github.ref == 'refs/heads/main'", condition, name)
                self.assertIn("github.event_name == 'push'", condition, name)
                self.assertIn("github.event_name == 'workflow_dispatch'", condition, name)
                self.assertNotIn('github.event.pull_request', condition, name)
            elif publisher:
                for guard in ("github.ref == 'refs/heads/main'", "github.event_name == 'workflow_dispatch'",
                              "github.event_name == 'workflow_run'", "github.event.workflow_run.conclusion == 'success'",
                              "github.event.workflow_run.head_repository.id == github.event.repository.id"):
                    self.assertIn(guard, condition, name)
            elif not contributor_policy:
                self.assertIn(
                    'github.event.pull_request.head.repo.id == github.event.repository.id',
                    condition,
                    name,
                )

    def test_code_running_jobs_keep_the_repository_identity_guard(self):
        for path in workflow_paths(WORKFLOW.parent):
            with self.subTest(workflow=path.name):
                self.assert_repository_identity_guards(
                    path.read_text(), contributor_policy=path.name == 'contributor-policy.yml',
                    main_only=path.name == 'ci.yml', publisher=path.name == 'npm-publish.yml',
                )

    def test_no_lane_depends_on_repository_visibility(self):
        for path in workflow_paths(WORKFLOW.parent):
            with self.subTest(workflow=path.name):
                source = path.read_text()
                self.assertNotIn('repository.private', source)
                self.assertNotIn('repository.visibility', source)

    def test_non_lowercase_and_quoted_jobs_cannot_omit_the_guards(self):
        for name in ['_build', 'Build', 'Build_2-x', '"Build"', "'_build'"]:
            source = f'jobs:\n  {name}:\n    runs-on: ubuntu-24.04\n'
            with self.subTest(job=name), self.assertRaises(AssertionError):
                self.assert_repository_identity_guards(source)

    def test_quoted_runner_and_condition_fields_cannot_hide_missing_guards(self):
        source = 'jobs:\n  Build:\n    "runs-on": ubuntu-24.04\n    \'if\': true\n'
        with self.assertRaises(AssertionError):
            self.assert_repository_identity_guards(source)

    def test_an_unrecognized_job_cannot_be_treated_as_a_reusable_caller(self):
        for source in ['jobs:\n  Build:\n    name: No runner\n',
                       'jobs:\n  Build:\n    uses: external/workflow@main\n']:
            with self.subTest(source=source), self.assertRaises(AssertionError):
                self.assert_repository_identity_guards(source)

    def test_privileged_contributor_policy_never_checks_out_candidate_code(self):
        policy = (WORKFLOW.parent / 'contributor-policy.yml').read_text()
        self.assertIn('pull_request_target:', policy)
        self.assertNotRegex(policy, re.compile(r'^\s*uses:', re.MULTILINE))
        self.assertNotIn('actions/checkout@', policy)
        self.assertNotIn('github.event.pull_request.head.ref', policy)
        self.assertNotIn('github.event.pull_request.title', policy)
        self.assertNotIn('github.event.pull_request.body', policy)

    def test_contributor_policy_uses_only_endpoint_driven_minimal_permissions(self):
        policy = (WORKFLOW.parent / 'contributor-policy.yml').read_text()
        value, body = block_mapping(policy, 0)['permissions']
        self.assertFalse(value.strip())
        permissions = block_mapping(body, 2)
        self.assertEqual(
            {key: item[0].strip() for key, item in permissions.items()},
            {'contents': 'read', 'pull-requests': 'read', 'statuses': 'write'},
        )
        self.assertTrue(all(not item[1].strip() for item in permissions.values()))
        self.assertIn('/pulls/$' + '{DISPATCH_PR_NUMBER}', policy)
        self.assertIn('/statuses/$' + '{CANDIDATE_SHA}', policy)
        self.assertNotIn('/pulls?', policy)

    def test_package_leaves_the_runner_only_as_a_named_workflow_artifact(self):
        package_job = jobs(WORKFLOW.read_text())['wasm']
        # Hosted runners are disposable: there is no host mount to fall back to.
        self.assertNotIn('PINEWASM_ARTIFACT_DIR', package_job)
        self.assertNotIn('mountpoint', package_job)
        self.assertIn('Archive identifier: ${artifact_name}', package_job)
        upload = package_job.index('- name: Retain verified Wasm package as a workflow artifact')
        step = package_job[upload:package_job.index('\n      - name:', upload)]
        self.assertIn('uses: actions/upload-artifact@', step)
        self.assertIn('if-no-files-found: error', step)
        self.assertNotIn('continue-on-error', step)

    def test_fresh_package_runs_the_generated_library_boundary_regression(self):
        package_job = jobs(WORKFLOW.read_text())['wasm']
        build = package_job.index(' -- setup')
        boundary = package_job.index(' -- wasm-test')
        retention = package_job.index('Retain verified Wasm package')
        self.assertLess(build, boundary)
        self.assertLess(boundary, retention)

    def test_all_feature_build_releases_finished_default_outputs(self):
        rust = jobs(WORKFLOW.read_text())['rust']
        default = rust.index('- name: Test Rust workspace and embedding example')
        cleanup = rust.index('- name: Release completed native build outputs before the all-feature build')
        all_features = rust.index('- name: Test all-feature and minimal-feature configurations')
        self.assertLess(default, cleanup)
        self.assertLess(cleanup, all_features)
        step = rust[cleanup:all_features]
        self.assertIn("if: steps.reuse.outputs.reused != 'true' && (matrix.platform == 'linux-x64')", step)
        self.assertIn('run: cargo clean', step)
        self.assertIn("CARGO_PROFILE_DEV_DEBUG: '0'", rust)
        self.assertIn("CARGO_PROFILE_TEST_DEBUG: '0'", rust)


# Offline tests execute the actual DCO/MSRV setup blocks. Keep fixture
# names separate from the setup-order checks above.
BOOTSTRAP_WORKFLOW = WORKFLOW


def BOOTSTRAP_setup_script(job, step):
    text = BOOTSTRAP_WORKFLOW.read_text()
    match = re.search(r"^  " + re.escape(job) + r":\n(.*?)(?=^  [\w-]+:|\Z)", text, re.M | re.S)
    if match is None:
        raise ValueError("missing job: " + job)
    block = match.group(1)
    marker = "      - name: " + step + "\n        run: |\n"
    if block.count(marker) != 1:
        raise ValueError("missing or ambiguous setup: " + step)
    lines = block.split(marker, 1)[1].splitlines()
    shell = []
    for line in lines:
        if line and not line.startswith("          "):
            break
        shell.append(line[10:] if line else "")
    return "\n".join(shell) + "\n"


BOOTSTRAP_RUSTUP = '''#!/bin/bash
printf 'rustup:%s\\n' "$*" >> "$MOCK_LOG"
if [ "$1" = toolchain ]; then exit "${MOCK_TOOLCHAIN_EXIT:-0}"; fi
if [ "$1" = target ]; then exit "${MOCK_TARGET_EXIT:-0}"; fi
exit 99
'''
BOOTSTRAP_INSTALLER = '''#!/bin/bash
printf 'installer:%s\\n' "$*" >> "$MOCK_LOG"
if [ "${MOCK_INSTALL_EXIT:-0}" -ne 0 ]; then exit "$MOCK_INSTALL_EXIT"; fi
mkdir -p "$EXPECTED_CARGO_BIN"
cp "$MOCK_RUSTUP_SOURCE" "$EXPECTED_CARGO_BIN/rustup"
chmod +x "$EXPECTED_CARGO_BIN/rustup"
'''
BOOTSTRAP_CURL = '''#!/bin/bash
printf 'curl:%s\\n' "$*" >> "$MOCK_LOG"
if [ "${MOCK_DOWNLOAD_EXIT:-0}" -ne 0 ]; then exit "$MOCK_DOWNLOAD_EXIT"; fi
if [ "$#" -ne 9 ] || [ "$1" != --proto ] || [ "$2" != '=https' ] || [ "$3" != --tlsv1.2 ] || [ "$4" != --fail ] || [ "$5" != --silent ] || [ "$6" != --show-error ] || [ "$7" != https://sh.rustup.rs ] || [ "$8" != --output ]; then exit 98; fi
cp "$MOCK_INSTALLER_SOURCE" "$9"
'''


class BootstrapTests(unittest.TestCase):
    jobs = [("dco", "Install pinned Rust toolchain"), ("msrv", "Install the declared minimum toolchain")]

    def run_setup(self, job, step, existing=True, custom_home=False, **failures):
        with tempfile.TemporaryDirectory(prefix="policy-msrv-bootstrap-") as directory:
            root = Path(directory)
            home = root / "home"
            cargo_home = root / "custom-cargo" if custom_home else home / ".cargo"
            cargo_bin = cargo_home / "bin"
            cargo_bin.mkdir(parents=True)
            mocks = root / "mocks"
            mocks.mkdir()
            tools = root / "tools"
            tools.mkdir()
            for name in ["sh", "cp", "chmod", "mkdir", "grep", "cut"]:
                executable = shutil.which(name)
                if executable is None:
                    raise RuntimeError("missing fixture prerequisite: " + name)
                (tools / name).symlink_to(executable)
            self.assertEqual({path.name for path in tools.iterdir()}, {"sh", "cp", "chmod", "mkdir", "grep", "cut"})
            self.assertFalse((tools / "rustup").exists())
            self.assertFalse((tools / "curl").exists())
            for name, content in [("curl", BOOTSTRAP_CURL), ("rustup-source", BOOTSTRAP_RUSTUP), ("installer-source", BOOTSTRAP_INSTALLER)]:
                path = mocks / name
                path.write_text(content)
                path.chmod(0o755)
            if existing:
                path = cargo_bin / "rustup"
                path.write_text(BOOTSTRAP_RUSTUP)
                path.chmod(0o755)
            (root / "Cargo.toml").write_text('[workspace.package]\nrust-version = "1.86"\n')
            github_path = root / "github-path"
            github_env = root / "github-env"
            github_path.touch()
            github_env.touch()
            # No inherited Bash startup hooks or broad system PATH: only the
            # explicit command whitelist and fixture doubles are reachable.
            env = {}
            env.update(HOME=str(home), PATH=str(mocks) + ":" + str(tools), RUNNER_TEMP=str(root), GITHUB_PATH=str(github_path), GITHUB_ENV=str(github_env), PINEWASM_RUST_VERSION="1.97.1", MOCK_LOG=str(root / "events"), EXPECTED_CARGO_BIN=str(cargo_bin), MOCK_RUSTUP_SOURCE=str(mocks / "rustup-source"), MOCK_INSTALLER_SOURCE=str(mocks / "installer-source"))
            if custom_home:
                env["CARGO_HOME"] = str(cargo_home)
            env.update({"MOCK_" + key.upper() + "_EXIT": str(value) for key, value in failures.items()})
            result = subprocess.run(["/bin/bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", BOOTSTRAP_setup_script(job, step)], cwd=root, env=env, capture_output=True, text=True)
            events = (root / "events").read_text().splitlines() if (root / "events").exists() else []
            return result, events, github_path.read_text(), github_env.read_text(), str(cargo_bin)

    def test_existing_binary_in_home_or_custom_cargo_bin(self):
        for job, step in self.jobs:
            for custom in [False, True]:
                with self.subTest(job=job, custom_home=custom):
                    result, events, path, _, cargo_bin = self.run_setup(job, step, custom_home=custom)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertFalse(any(line.startswith(("curl:", "installer:")) for line in events))
                    self.assertEqual(path, cargo_bin + "\n")
                    self.assert_tools(job, events)

    def assert_tools(self, job, events):
        calls = [line for line in events if line.startswith("rustup:")]
        if job == "dco":
            self.assertEqual(calls, ["rustup:toolchain install 1.97.1 --profile minimal"])
        else:
            self.assertEqual(calls, [
                "rustup:toolchain install 1.97.1 --profile minimal",
                "rustup:toolchain install 1.86 --profile minimal",
                "rustup:target add wasm32-unknown-unknown --toolchain 1.86",
            ])

    def test_missing_binary_official_installer_then_exact_pins(self):
        for job, step in self.jobs:
            for custom in [False, True]:
                with self.subTest(job=job, custom_home=custom):
                    result, events, path, github_env, cargo_bin = self.run_setup(job, step, existing=False, custom_home=custom)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertTrue(events[0].startswith("curl:"))
                    self.assertEqual(events[1], "installer:-y --no-modify-path --profile minimal --default-toolchain none")
                    self.assertEqual(path, cargo_bin + "\n")
                    self.assert_tools(job, events)
                    self.assertEqual(github_env, "PINEWASM_RUST_MSRV=1.86\n" if job == "msrv" else "")

    def test_download_and_installer_failures_stop_before_toolchain(self):
        for job, step in self.jobs:
            for failure, code in [("download", 22), ("install", 23)]:
                with self.subTest(job=job, failure=failure):
                    result, events, path, _, _ = self.run_setup(job, step, existing=False, **{failure: code})
                    self.assertEqual(result.returncode, code)
                    self.assertFalse(any(line.startswith("rustup:") for line in events))
                    self.assertEqual(path, "")
                    if failure == "download":
                        self.assertFalse(any(line.startswith("installer:") for line in events))

    def test_toolchain_failure_prevents_later_target(self):
        for job, step in self.jobs:
            with self.subTest(job=job):
                result, events, _, _, _ = self.run_setup(job, step, toolchain=24)
                self.assertEqual(result.returncode, 24)
                self.assertFalse(any(line.startswith("rustup:target") for line in events))

    def test_msrv_target_failure_propagates(self):
        result, events, _, _, _ = self.run_setup("msrv", "Install the declared minimum toolchain", target=25)
        self.assertEqual(result.returncode, 25)
        self.assertEqual(events[-1], "rustup:target add wasm32-unknown-unknown --toolchain 1.86")

    def test_shared_bootstrap_matches_native_job(self):
        text = BOOTSTRAP_WORKFLOW.read_text()
        begin = text.index("      # Bootstrap native runners")
        native = text[begin:]
        native_bootstrap = native[native.index("          set -euo pipefail"):native.index("          rustup toolchain install")]
        expected = "\n".join(line[10:] for line in native_bootstrap.rstrip().splitlines()) + "\n"
        for job, step in self.jobs:
            self.assertTrue(BOOTSTRAP_setup_script(job, step).startswith(expected), job)



if __name__ == "__main__":
    unittest.main()
