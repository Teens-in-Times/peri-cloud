"""Persistent issue lifecycle, archival, ownership, and stale-run boundaries."""
import copy
import unittest
import sys
from pathlib import Path
from unittest.mock import patch
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import upstream_tracking as tracking


def report(state='conflict', head='b'*40):
    return dict(state=state,base_commit='a'*40,upstream_base='c'*40,upstream_head=head,conflicts=[{'path':'fixture.rs','reason':'conflict'}],conflict_count=1)


class Remote:
    repository='fixture/project'
    def __init__(self, issues=()):
        self.issues=copy.deepcopy(list(issues));self.writes=[];self.main='a'*40
    def api(self,path,data=None,missing=False,method=None):
        if path=='git/ref/heads/main':return {'object':{'sha':self.main}}
        if path.startswith('issues?'):
            assert data is None
            return copy.deepcopy(self.issues)
        self.writes.append((path,copy.deepcopy(data),method))
        if path=='issues':
            assert method is None
            issue={'number':40,'html_url':'https://example.invalid/issues/40','user':{'login':'github-actions[bot]'},'state':'open',**data}
            self.issues.append(issue);return copy.deepcopy(issue)
        assert method=='PATCH'
        issue=next(i for i in self.issues if path==f'issues/{i["number"]}')
        issue.update(data);return copy.deepcopy(issue)


def legacy(number,user='github-actions[bot]'):
    return dict(number=number,html_url=f'https://example.invalid/issues/{number}',user={'login':user},state='open',title='old report',body=tracking.LEGACY+'c:b -->\nOriginal evidence')


class TrackingTests(unittest.TestCase):
    def test_latest_head_refreshes_same_issue_retry_is_idempotent_and_recovery_is_visible(self):
        remote=Remote()
        with patch.dict('os.environ',{'GITHUB_RUN_ID':'123'}):
            tracking.update_tracker(remote,report(),'reported')
            self.assertEqual(len(remote.issues),1)
            self.assertEqual(len(remote.writes),1)
            tracking.update_tracker(remote,report(),'reported')
            self.assertEqual(len(remote.writes),1)
            remote.issues[0]['body']+='\nOperator notes stay here.\n'
            tracking.update_tracker(remote,report(head='d'*40),'reported')
            self.assertEqual(len(remote.issues),1)
            self.assertIn('d'*40,remote.issues[0]['body'])
            self.assertIn('Operator notes',remote.issues[0]['body'])
            remote.issues[0]['state']='closed'
            tracking.update_tracker(remote,report(),'reported')
            self.assertEqual(remote.issues[0]['state'],'open')
            tracking.update_tracker(remote,report('unchanged'),'unchanged')
            self.assertIn('本次无变化',remote.issues[0]['body'])
            self.assertEqual(remote.issues[0]['state'],'open')
            tracking.update_tracker(remote,report('prepared'),'pending')
            self.assertIn('已有候选等待处理',remote.issues[0]['body'])

    def test_legacy_reports_archive_without_overwriting_human_issue_or_original_evidence(self):
        remote=Remote([legacy(30),legacy(31),legacy(32),legacy(99,'human')])
        tracking.update_tracker(remote,report(),'reported')
        self.assertEqual(len(remote.issues),4)
        for issue in remote.issues:
            if issue['number'] in (30,31):
                self.assertEqual(issue['state'],'closed')
                self.assertTrue(issue['body'].startswith(tracking.LEGACY))
            elif issue['number']==32:
                self.assertTrue(issue['body'].startswith(tracking.TRACKER))
                self.assertIn('Original evidence',issue['body'])
                self.assertIn('https://example.invalid/issues/30',issue['body'])
            else:self.assertEqual(issue,legacy(99,'human'))
        # A delayed old publisher can leave another legacy report; keep its link too.
        remote.issues.append(legacy(41))
        tracking.update_tracker(remote,report(),'reported')
        self.assertEqual(remote.issues[-1]['state'],'closed')
        self.assertIn('/issues/41',remote.issues[2]['body'])

    def test_stale_runs_malformed_markers_and_healthy_first_run_cannot_create_noise(self):
        remote=Remote();remote.main='f'*40
        self.assertEqual(tracking.update_tracker(remote,report(),'reported'),'stale')
        self.assertEqual(remote.writes,[])
        remote.main='a'*40
        self.assertEqual(tracking.update_tracker(remote,report('unchanged'),'unchanged'),'absent')
        self.assertEqual(remote.writes,[])
        row=legacy(32);row['body']=tracking.TRACKER+'\nOperator custom body without region markers'
        remote=Remote([row])
        with self.assertRaises(ValueError):tracking.update_tracker(remote,report(),'reported')
        self.assertEqual(remote.writes,[])


if __name__=='__main__':unittest.main()
