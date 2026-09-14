// NIP-68: picture-first feeds. A kind-20 event carries its image(s) in NIP-92 `imeta` tags and a
// caption in content. Kind constant lives here (its NIP module), mirroring KIND_ARTICLE (nip23) and
// KIND_POLL (nip88) - so every named kind has one home.
export const KIND_PICTURE = 20;

/** The caption's first non-empty line, capped. A blank picture title is derived from this (signPicture),
 * and the card suppresses a title that equals it (pictureBody) - ONE definition so the derive + the
 * suppress stay in lockstep (change the rule here, both sides move together). */
export const firstCaptionLine = (content: string, cap = 120): string =>
    (content.split('\n').map((l) => l.trim()).find(Boolean) ?? '').slice(0, cap);
