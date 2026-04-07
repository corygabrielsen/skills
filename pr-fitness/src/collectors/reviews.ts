import type { GhReview } from "../types/index.js";
import { gh } from "../util/gh.js";

export async function collectReviews(
  repo: string,
  pr: number,
): Promise<readonly GhReview[]> {
  return gh<GhReview[]>([
    "api",
    `repos/${repo}/pulls/${String(pr)}/reviews`,
    "--jq",
    "[.[] | {user: .user.login, state, commit_id, submitted_at}]",
  ]);
}
