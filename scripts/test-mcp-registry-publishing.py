#!/usr/bin/env python3
"""Offline registry publication guard regressions. No credentials or OIDC calls."""
import base64
import copy
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import re
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('registry_release', ROOT / 'scripts/prepare-mcp-registry-release.py')
registry = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(registry)
VERSION = '12.2.4'
COMMIT = 'a' * 40
MANIFEST = json.loads((ROOT / 'server.json').read_text())
PACKAGE = json.loads((ROOT / 'npm/package.json').read_text())
# Fixtures track any later repository version while remaining immutable test data.
MANIFEST['version'] = MANIFEST['packages'][0]['version'] = PACKAGE['version'] = VERSION
PACKAGE['optionalDependencies'] = {f'nestweaver-{p}': VERSION for p in registry.PLATFORMS}


class Fixture:
    def __init__(self):
        self.manifest = copy.deepcopy(MANIFEST)
        self.package = copy.deepcopy(PACKAGE)
        self.tag = {'object': {'type': 'commit', 'sha': COMMIT}}
        self.release = {'tag_name': 'v' + VERSION, 'draft': False,
                        'prerelease': False, 'published_at': '2026-10-28T18:03:47Z'}
        self.entries = {}
        self.calls = []
        self.pack()

    def pack(self, packed=None):
        stream = io.BytesIO()
        with tarfile.open(fileobj=stream, mode='w:gz') as archive:
            data = json.dumps(packed or self.package).encode()
            info = tarfile.TarInfo('package/package.json')
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))
        self.tarball = stream.getvalue()
        self.metadata = {}
        for name in ('nestweaver', *(f'nestweaver-{p}' for p in registry.PLATFORMS)):
            self.metadata[name] = {'name': name, 'version': VERSION, 'mcpName': registry.NAME,
                'dist': {'integrity': 'sha512-' + base64.b64encode(hashlib.sha512(self.tarball).digest()).decode(),
                         'shasum': hashlib.sha1(self.tarball).hexdigest(),
                         'tarball': f'https://registry.npmjs.org/{name}/-/{name}-{VERSION}.tgz'}}

    def github(self, path):
        self.calls.append(path)
        if path.startswith('git/ref/'):
            return self.tag
        if path.startswith('git/tags/'):
            return {'object': {'type': 'commit', 'sha': COMMIT}}
        if path.startswith('releases/tags/'):
            return self.release
        if path.startswith('contents/'):
            assert path.endswith('?ref=' + COMMIT), path
            value = self.manifest if path.startswith('contents/server.json?') else self.package
            return {'type': 'file', 'encoding': 'base64',
                    'content': base64.b64encode(json.dumps(value).encode()).decode()}
        raise AssertionError(path)

    def public(self, url, missing=False):
        self.calls.append(url)
        if url.startswith('https://registry.npmjs.org/'):
            return self.metadata[url.split('/')[3]]
        assert missing and url.startswith(registry.REGISTRY + '/v0.1/servers/'), url
        return self.entries.get(url)

    def download(self, url):
        self.calls.append(url)
        assert url == f'https://registry.npmjs.org/nestweaver/-/nestweaver-{VERSION}.tgz', url
        return self.tarball

    def prepare(self, directory, expected=None):
        return registry.prepare(VERSION, COMMIT, Path(directory), expected,
                                self.github, self.public, self.download)


class ReleasePreparation(unittest.TestCase):
    def test_exact_release_is_read_from_commit_and_bytes_preserved(self):
        fixture = Fixture()
        with tempfile.TemporaryDirectory() as directory:
            receipt = fixture.prepare(directory)
            data = (Path(directory) / 'server.json').read_bytes()
            self.assertEqual(data, json.dumps(fixture.manifest).encode())
            self.assertEqual(receipt['manifest_sha256'], hashlib.sha256(data).hexdigest())
            self.assertTrue(receipt['should_publish'])
            self.assertFalse(any('oidc' in call or 'publish' in call for call in fixture.calls))
            self.assertEqual(fixture.prepare(directory, receipt['manifest_sha256']), receipt)
            with self.assertRaisesRegex(ValueError, 'manifest changed'):
                fixture.prepare(directory, 'b' * 64)

    def test_rejects_injection_ranges_prerelease_and_unpinned_commits(self):
        for version in ['latest', '^12.2.4', '12.2.4;echo bad', '../main', '12.2.4-rc.1', '012.2.4', '12.2.4\n']:
            with self.subTest(version=version), self.assertRaises(ValueError):
                registry.validate_inputs(version, COMMIT)
        for commit in ['main', 'v12.2.4', COMMIT[:8], 'A' * 40, COMMIT + '\n']:
            with self.subTest(commit=commit), self.assertRaises(ValueError):
                registry.validate_inputs(VERSION, commit)

    def test_rejects_wrong_tag_and_unpublished_release(self):
        fixture = Fixture()
        fixture.tag['object']['sha'] = 'b' * 40
        with self.assertRaisesRegex(ValueError, 'expected SHA'):
            registry.release_commit(VERSION, COMMIT, fixture.github)
        fixture = Fixture()
        for changes in [{'draft': True}, {'prerelease': True}, {'published_at': None}, {'tag_name': 'v0.0.0'}]:
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                fixture.release = dict(tag_name='v' + VERSION, draft=False, prerelease=False,
                                       published_at='now') | changes
                registry.release_commit(VERSION, COMMIT, fixture.github)

    def test_annotated_tag_is_peeled_but_tag_cycles_refuse(self):
        fixture = Fixture()
        fixture.tag = {'object': {'type': 'tag', 'sha': 'b' * 40}}
        registry.release_commit(VERSION, COMMIT, fixture.github)
        with self.assertRaisesRegex(ValueError, 'too deep'):
            registry.release_commit(VERSION, COMMIT, lambda _: fixture.tag)

    def test_identity_and_launch_changes_fail_closed(self):
        mutations = [lambda m: m.update(name='io.github.someone/other'),
                     lambda m: m.update(version='0.0.0'),
                     lambda m: m['repository'].update(url='https://example.invalid'),
                     lambda m: m['packages'][0].update(version='0.0.0'),
                     lambda m: m['packages'][0].update(registryBaseUrl='https://example.invalid'),
                     lambda m: m['packages'][0]['packageArguments'][0].update(value='daemon'),
                     lambda m: m['packages'][0]['packageArguments'][1].update(default='/tmp/db'),
                     lambda m: m['packages'][0]['packageArguments'][1].update(isRequired=False),
                     lambda m: m['packages'][0].update(runtimeArguments=[{'value': '--evil'}])]
        for mutate in mutations:
            manifest = copy.deepcopy(MANIFEST)
            mutate(manifest)
            with self.subTest(manifest=manifest), self.assertRaises(ValueError):
                registry.validate_manifest(manifest, PACKAGE, VERSION)

    def test_npm_missing_wrong_ownership_integrity_and_tarball_mismatch(self):
        for mutation in [lambda f: f.metadata['nestweaver'].update(mcpName='other'),
                         lambda f: f.metadata['nestweaver-linux-x64'].update(version='0.0.0'),
                         lambda f: f.metadata['nestweaver']['dist'].update(tarball='https://example.invalid'),
                         lambda f: f.metadata['nestweaver']['dist'].update(integrity='sha512-' + base64.b64encode(b'a' * 64).decode()),
                         lambda f: f.metadata['nestweaver']['dist'].update(shasum='b' * 40),
                         lambda f: f.pack(dict(f.package, mcpName='other'))]:
            fixture = Fixture()
            mutation(fixture)
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                registry.validate_npm(fixture.package, VERSION, fixture.public, fixture.download)

    def test_registry_existing_version_is_noop_and_conflict_refuses(self):
        fixture = Fixture()
        fixture.entries = {registry.endpoint(VERSION): {'server': fixture.manifest},
                           registry.endpoint('latest'): {'server': fixture.manifest}}
        self.assertFalse(registry.registry_state(fixture.manifest, fixture.public, True))
        fixture.entries[registry.endpoint(VERSION)] = {'server': dict(fixture.manifest, title='Different')}
        with self.assertRaisesRegex(ValueError, 'differs'):
            registry.registry_state(fixture.manifest, fixture.public)

    def test_registry_rejects_extra_functional_fields_but_accepts_response_metadata(self):
        fixture = Fixture()
        exact = {'server': fixture.manifest, '_meta': {'io.modelcontextprotocol.registry/official': {'isLatest': True}}}
        fixture.entries = {registry.endpoint(VERSION): exact, registry.endpoint('latest'): exact}
        self.assertFalse(registry.registry_state(fixture.manifest, fixture.public, True))
        for url in [registry.endpoint(VERSION), registry.endpoint('latest')]:
            for extra in [{'remotes': [{'type': 'streamable-http', 'url': 'https://example.invalid/mcp'}]},
                          {'_meta': {'example.invalid/config': {'command': 'unexpected'}}}]:
                with self.subTest(url=url, extra=extra), self.assertRaisesRegex(ValueError, 'differs'):
                    fixture.entries = {registry.endpoint(VERSION): exact, registry.endpoint('latest'): exact}
                    fixture.entries[url] = {'server': dict(fixture.manifest, **extra)}
                    registry.registry_state(fixture.manifest, fixture.public, True)

    def test_verification_binds_receipt_manifest_bytes_and_selected_commit(self):
        fixture = Fixture()
        fixture.entries = {registry.endpoint(VERSION): {'server': fixture.manifest},
                           registry.endpoint('latest'): {'server': fixture.manifest}}
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            fixture.prepare(directory)
            self.assertEqual(registry.verify_prepared(VERSION, COMMIT, output, fixture.public),
                             {'verified_version': registry.endpoint(VERSION), 'latest': registry.endpoint('latest')})
            with self.assertRaisesRegex(ValueError, 'receipt does not match'):
                registry.verify_prepared(VERSION, 'b' * 40, output, fixture.public)
            (output / 'server.json').write_text(json.dumps(dict(fixture.manifest, title='Changed')))
            with self.assertRaisesRegex(ValueError, 'bytes changed'):
                registry.verify_prepared(VERSION, COMMIT, output, fixture.public)

    def test_missing_verification_and_downgrade_fail(self):
        fixture = Fixture()
        with self.assertRaisesRegex(ValueError, 'still absent'):
            registry.registry_state(fixture.manifest, fixture.public, True)
        fixture.entries[registry.endpoint('latest')] = {'server': dict(fixture.manifest, version='99.0.0')}
        with self.assertRaisesRegex(ValueError, 'older version'):
            registry.registry_state(fixture.manifest, fixture.public)


class WorkflowBoundary(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workflow = (ROOT / '.github/workflows/publish-mcp-registry.yml').read_text()
        cls.jobs = dict(re.findall(r'^  ([a-z-]+):\n(.*?)(?=^  [a-z-]+:\n|\Z)',
                                  cls.workflow.split('\njobs:\n', 1)[1], re.M | re.S))

    def test_only_manual_main_repository_workflow_has_publishing_permission(self):
        triggers = self.workflow.split('\non:\n', 1)[1].split('\npermissions:', 1)[0]
        self.assertEqual(re.findall(r'^  ([a-z_]+):', triggers, re.M), ['workflow_dispatch'])
        self.assertIn('\npermissions: {}\n', self.workflow)
        self.assertEqual(set(self.jobs), {'prepare', 'publish'})
        for body in self.jobs.values():
            for guard in ["github.repository == 'Kehl-io/nestweaver'", "github.event_name == 'workflow_dispatch'",
                          "github.ref == 'refs/heads/main'", 'publish-mcp-registry.yml@refs/heads/main']:
                self.assertIn(guard, body)
            self.assertIn('ref: ${{ github.sha }}', body)
            self.assertIn('persist-credentials: false', body)
        self.assertEqual(self.workflow.count('id-token: write'), 1)
        self.assertNotIn('id-token:', self.jobs['prepare'])
        self.assertNotIn('secrets.', self.workflow)
        self.assertNotIn('npm publish', self.workflow)

    def test_validation_precedes_oidc_and_publisher_is_pinned(self):
        publish = self.jobs['publish']
        self.assertLess(publish.index('Revalidate exact release'), publish.index('login github-oidc'))
        self.assertLess(publish.index(' validate '), publish.index('login github-oidc'))
        self.assertLess(publish.index('login github-oidc'), publish.index(' publish '))
        self.assertIn('--expected-manifest-sha256 "$MANIFEST_SHA256"', publish)
        self.assertIn('/download/v1.8.6/mcp-publisher_linux_amd64.tar.gz', publish)
        self.assertIn('1fbe93fc0783c2394d8012c8d6e50a59e98cb2cc2d216264cc69f405d0e21340', publish)
        self.assertIn('sha256sum --check', publish)
        self.assertIn('--verify-published', publish)
        self.assertIn('if: always()', publish)
        self.assertIn('"$MCP_PUBLISHER" logout', publish)
        self.assertIn('cancel-in-progress: false', self.workflow)
        self.assertIn('python3 scripts/test-mcp-registry-publishing.py', (ROOT / '.github/workflows/ci.yml').read_text())


if __name__ == '__main__':
    unittest.main(verbosity=2)
