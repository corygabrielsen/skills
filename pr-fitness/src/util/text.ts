/** Simple pluralization: count + singular + plural (default plural = singular + "s"). */
export function pluralize(n: number, singular: string, plural?: string): string {
  return `${String(n)} ${n === 1 ? singular : (plural ?? `${singular}s`)}`;
}
