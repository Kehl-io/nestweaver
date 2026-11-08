#!/usr/bin/env python3
"""Offline merge-title policy fixture; no GitHub or release mutation."""
import importlib.util
from pathlib import Path
import re
import unittest

spec = importlib.util.spec_from_file_location('policy', Path(__file__).with_name('check-pr-title.py'))
policy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(policy)


class Policy(unittest.TestCase):
    def event(self, title, branch='fix/example'):
        return {'pull_request': {'title': title, 'head': {'ref': branch}}}

    def test_two_changes_gain_no_synthetic_third_entry(self):
        title = 'Preserve valid seed results and repair policy recovery'
        changes = ['fix(mcp): preserve valid seeds', 'fix(engine): reconcile ignore policy']
        policy.check(self.event(title))
        subjects = changes + [title]
        self.assertEqual([s for s in subjects if policy.CONVENTIONAL.match(s)], changes)
        with self.assertRaises(ValueError):
            policy.check(self.event('fix: preserve seed results and policy recovery'))

    def test_release_exception_is_bounded(self):
        policy.check(self.event('chore(main): release 1.2.3', 'release-please--branches--main'))
        for title, branch in [('chore(main): release 1.2.3', 'fix/example'),
                              ('fix: unrelated', 'release-please--branches--main')]:
            with self.assertRaises(ValueError): policy.check(self.event(title, branch))
        policy.check({})

    def test_breaking_and_scoped_titles_are_also_conventional(self):
        for title in ['fix!: breaking', 'fix(mcp)!: breaking', 'docs: improve help']:
            with self.assertRaises(ValueError): policy.check(self.event(title))

    def test_single_exact_commit_uses_existing_dedupe_without_bot_title_churn(self):
        event = self.event('chore(deps): update dependency')
        event['pull_request']['commits'] = 1
        policy.check(event, 'chore(deps): update dependency')
        with self.assertRaises(ValueError): policy.check(event, 'fix: different change')
        event['pull_request']['commits'] = 2
        with self.assertRaises(ValueError): policy.check(event, 'chore(deps): update dependency')


if __name__ == '__main__':
    unittest.main()
