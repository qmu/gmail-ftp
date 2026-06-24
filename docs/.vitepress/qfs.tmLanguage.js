// A Shiki/TextMate grammar for the qfs pipe-SQL language, so ```qfs code blocks highlight
// keywords, the |> pipe, paths, strings, numbers, and comments. Registered in config.mts.
export const qfsGrammar = {
  name: 'qfs',
  scopeName: 'source.qfs',
  patterns: [
    { include: '#comment' },
    { include: '#string' },
    { include: '#keyword' },
    { include: '#pipe' },
    { include: '#call' },
    { include: '#path' },
    { include: '#number' },
    { include: '#operator' },
  ],
  repository: {
    comment: {
      patterns: [
        { match: '--.*$', name: 'comment.line.double-dash.qfs' },
        { match: '#.*$', name: 'comment.line.number-sign.qfs' },
      ],
    },
    string: { match: "'[^']*'", name: 'string.quoted.single.qfs' },
    // Longer / multi-word keywords first (Oniguruma alternation is ordered, not longest-match).
    keyword: {
      match:
        '(?i)\\b(INSERT INTO|UPSERT INTO|MATERIALIZED VIEW|GROUP BY|ORDER BY|CREATE|ENDPOINT|TRIGGER|JOB|VIEW|WEBHOOK|POLICY|FROM|WHERE|SELECT|EXTEND|SET|AGGREGATE|LIMIT|DISTINCT|JOIN|UNION|EXCEPT|INTERSECT|EXPAND|UPDATE|REMOVE|VALUES|RETURNING|CALL|DECODE|ENCODE|PREVIEW|COMMIT|ALLOW|DENY|EVERY|DO|ON|AS|AND|OR|NOT|IN|ANY|BETWEEN|LIKE|ASC|DESC|NEW)\\b',
      name: 'keyword.control.qfs',
    },
    pipe: { match: '\\|>', name: 'keyword.operator.pipe.qfs' },
    // service.action in a CALL, e.g. mail.send, github.merge
    call: {
      match: '\\b([a-z][a-z0-9_]*)\\.([a-z][a-z0-9_]*)\\b',
      name: 'support.function.qfs',
    },
    // Absolute paths: /mail/inbox, /sql/pg/orders, /git/repo@v1.2/src
    path: { match: '/[A-Za-z0-9_][A-Za-z0-9_./@*\\-]*', name: 'entity.name.tag.path.qfs' },
    number: { match: '\\b[0-9]+\\b', name: 'constant.numeric.qfs' },
    operator: { match: '(=>|<=|>=|<>|=|<|>|~)', name: 'keyword.operator.qfs' },
  },
}
