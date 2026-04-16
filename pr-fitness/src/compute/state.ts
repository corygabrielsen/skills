import type {
  GitHubPullRequestView,
  PullRequestState,
} from "../types/index.js";

export function computeState(
  pr: GitHubPullRequestView,
  lastCommitDate: string | null = null,
): PullRequestState {
  const title = pr.title;
  const prNum = pr.number;
  const body = pr.body ?? "";

  const suffixLen = ` (#${String(prNum)})`.length;
  const titleLen = title.length + suffixLen;

  const labels = pr.labels.map((l) => l.name);

  return {
    conflict: pr.mergeable,
    draft: pr.isDraft,
    wip: labels.includes("work in progress"),
    title_len: titleLen,
    title_ok: titleLen <= 50,
    body: body.length > 0,
    summary: /^## Summary/im.test(body),
    test_plan: /^## Test/im.test(body),
    content_label: labels.includes("bug") || labels.includes("enhancement"),
    assignees: pr.assignees.length,
    reviewers: pr.reviewRequests.length,
    merge_when_ready: labels.includes("merge-when-ready"),
    commits: pr.commits.length,
    updated_at: pr.updatedAt,
    last_commit_at: lastCommitDate,
  };
}
