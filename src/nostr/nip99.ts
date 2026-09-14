// NIP-99 classified listings. Kind 30402 is a published listing (30403 is a draft, not rendered).
// A listing packs price as ["price", amount, currency, frequency?], may carry multiple `image` tags,
// an optional `status` (e.g. "sold"), `location`, and Markdown in content. Hand-coded (graduated from
// the declarative engine) because that price formatting + gallery + sold badge + "message seller" are
// bespoke logic the generic archetype can't express. Kind home lives here, mirroring KIND_PICTURE.
export const KIND_LISTING = 30402;
