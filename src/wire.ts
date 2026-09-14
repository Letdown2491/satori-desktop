// Wire-contract types for the nip07-hateoas (hext.js) signing plugin protocol. Kept in
// their own dependency-free module so data-layer consumers can use them WITHOUT importing
// the HTTP kernel (http.ts), keeping the layering clean. Home for future wire types.

/** One slot of a `*_batch` continuation result: the order- and length-preserving array the
 * plugin POSTs back, one slot per item sent. `ok:false` = that slot was skipped/failed. */
export type BatchResult = { ok: true; value: unknown } | { ok: false };
