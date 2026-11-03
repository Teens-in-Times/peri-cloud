"""One persistent, bot-owned status issue; source snapshots remain explicit."""
from __future__ import annotations
import json
import os

TRACKER = '<!-- peri-upstream-tracker:v1 -->'
LEGACY = '<!-- peri-upstream-sync:'
START = '<!-- peri-upstream-status:start -->'
END = '<!-- peri-upstream-status:end -->'
TITLE = '[上游同步] 持续检测与阻塞跟踪'


def issue_pages(github):
    result = []
    for page in range(1, 11):
        rows = github.api(f'issues?state=all&sort=created&direction=desc&per_page=100&page={page}')
        result.extend(rows)
        if len(rows) < 100:
            return result
    raise ValueError('tracking history exceeds bounded scan')


def status_block(report, publication):
    state = report['state']
    labels = {'conflict': '同步受阻：需要人工适配', 'blocked': '同步受阻：需要人工审查',
              'unchanged': '本次无变化：叠加范围内无需同步',
              'prepared': '候选已准备：请检查 CI 并人工审查'}
    label = labels[state]
    if publication == 'pending':
        label = '已有候选等待处理：本次未另建 PR'
    elif publication == 'closed':
        label = '该候选已关闭：本次未重新创建'
    run = os.environ.get('GITHUB_RUN_ID', '')
    link = (f'https://github.com/{report["destination"]}/actions/runs/{run}'
            if run.isdecimal() else '本地验证（未提供运行链接）')
    details = {key: report[key] for key in ('state', 'reason', 'conflict_count', 'conflicts',
               'attribution_changes', 'scoped_changes') if key in report}
    details['publication'] = publication
    rendered = json.dumps(details, ensure_ascii=True, indent=2).replace('`', '\\u0060').replace('<', '\\u003c')
    return (f'{START}\n## 当前状态：{label}\n\n'
            f'本仓库快照：`{report["base_commit"]}`。\n\n'
            f'上游：`{report["upstream_base"]}` → `{report["upstream_head"]}`。\n\n'
            f'最近检测：{link}\n\n```json\n{rendered}\n```\n\n'
            '工作流成功表示检测与报告成功，不等于源码已同步、CI 已通过或已部署。'
            '有冲突时不创建源码 PR；候选仍需人工审查和合并。\n' + END)


def update_tracker(github, report, publication):
    # A delayed run may not replace a newer main's status or resurrect obsolete blockers.
    if github.api('git/ref/heads/main')['object']['sha'] != report['base_commit']:
        print('Tracking skipped: main advanced; next run will refresh status')
        return 'stale'
    rows = issue_pages(github)
    owned = [r for r in rows if not r.get('pull_request')
             and r.get('user', {}).get('login') == 'github-actions[bot]']
    trackers = [r for r in owned if TRACKER in (r.get('body') or '')]
    if len(trackers) > 1:
        raise ValueError('multiple canonical tracking issues; manual consolidation required')
    legacy = [r for r in owned if (r.get('body') or '').startswith(LEGACY)]
    issue = trackers[0] if trackers else max(legacy, key=lambda r: r['number'], default=None)
    blocked = report['state'] in ('blocked', 'conflict')
    if issue is None and not blocked:
        return 'absent'
    block = status_block({**report, 'destination': github.repository}, publication)
    if issue and TRACKER in (issue.get('body') or ''):
        previous = issue.get('body') or ''
        if previous.count(START) != 1 or previous.count(END) != 1:
            raise ValueError('tracking markers changed; refusing to overwrite operator notes')
        begin, end = previous.index(START), previous.index(END) + len(END)
        if begin >= end - len(END):
            raise ValueError('invalid tracking marker order')
        body = previous[:begin] + block + previous[end:]
        for old in legacy:
            if old['html_url'] not in body:
                body += f'\n- 历史报告 [#{old["number"]}]({old["html_url"]})\n'
    else:
        links = '\n'.join(f'- [#{r["number"]}]({r["html_url"]})' for r in legacy
                          if not issue or r['number'] != issue['number'])
        body = TRACKER + '\n\n' + block + '\n\n## 历史报告\n\n' + (links or '尚无历史报告。')
        if issue:
            body += '\n\n<details><summary>首次归并时的原始报告</summary>\n\n' + issue['body'] + '\n</details>\n'
    if len(body.encode('utf-8')) > 60000:
        raise ValueError('tracking issue exceeds bounded size')
    if issue is None:
        issue = github.api('issues', {'title': TITLE, 'body': body})
    else:
        changes = {}
        if body != issue.get('body'): changes['body'] = body
        if issue.get('title') != TITLE: changes['title'] = TITLE
        if blocked and issue.get('state') == 'closed': changes['state'] = 'open'
        if changes:
            # GitHub.api defaults to POST; issue updates explicitly use PATCH.
            github.api(f'issues/{issue["number"]}', changes, method='PATCH')
    # Preserve original report bodies and human comments; close duplicate snapshots only.
    for old in legacy:
        if old['number'] != issue['number'] and old.get('state') == 'open':
            github.api(f'issues/{old["number"]}', {'state': 'closed', 'state_reason': 'not_planned'}, method='PATCH')
    print('Tracking issue', issue['html_url'])
    return issue['html_url']
