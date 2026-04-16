import type { GraphiteCheck } from "../types/output.js";
import { gh } from "../util/gh.js";

interface GraphiteCheckRun {
  data: {
    repository: {
      pullRequest: {
        commits: {
          nodes: readonly {
            commit: {
              committedDate: string;
              statusCheckRollup: {
                contexts: {
                  nodes: readonly GraphiteNode[];
                };
              } | null;
            };
          }[];
        };
      };
    };
  };
}

export interface GraphiteCollectorResult {
  check: GraphiteCheck;
  lastCommitDate: string | null;
}

interface GraphiteNode {
  __typename: string;
  name?: string;
  status?: string;
  title?: string | null;
  summary?: string | null;
}

const GRAPHITE_CHECK_NAME = "Graphite / mergeability_check";

export async function collectGraphiteCheck(
  owner: string,
  name: string,
  pr: number,
): Promise<GraphiteCollectorResult> {
  const result = await gh<GraphiteCheckRun>([
    "api",
    "graphql",
    "-f",
    `query={
      repository(owner:"${owner}",name:"${name}") {
        pullRequest(number:${String(pr)}) {
          commits(last:1) {
            nodes {
              commit {
                committedDate
                statusCheckRollup {
                  contexts(first:100) {
                    nodes {
                      ... on CheckRun {
                        __typename
                        name
                        status
                        title
                        summary
                      }
                    }
                  }
                }
              }
            }
          }
        }
      }
    }`,
  ]);

  const commitNode = result.data.repository.pullRequest.commits.nodes[0];
  const lastCommitDate = commitNode?.commit.committedDate ?? null;
  const rollup = commitNode?.commit.statusCheckRollup;

  if (!rollup) {
    return {
      check: { status: "none", title: null, summary: null },
      lastCommitDate,
    };
  }

  const check = rollup.contexts.nodes.find(
    (n) => n.__typename === "CheckRun" && n.name === GRAPHITE_CHECK_NAME,
  );

  if (!check) {
    return {
      check: { status: "none", title: null, summary: null },
      lastCommitDate,
    };
  }

  const status = check.status === "COMPLETED" ? "pass" : "pending";

  return {
    check: {
      status,
      title: check.title ?? null,
      summary: check.summary ?? null,
    },
    lastCommitDate,
  };
}
