// NIP-71 video events. Two flavors x two addressability: a NORMAL (horizontal) video and a SHORT
// (vertical/reel) video, each as a regular event (21/22) and an addressable one (34235/34236). All
// share the same shape: title + a NIP-92 imeta describing the video (url, m, dim, poster `image`), with
// Markdown-ish summary in content. Hand-coded (not declarative) because the privacy-aware video player
// is bespoke logic. Kind home here, like KIND_PICTURE (nip68) / KIND_PODCAST_EPISODE (nipf4).
export const KIND_VIDEO_NORMAL = 21;
export const KIND_VIDEO_SHORT = 22;
export const KIND_VIDEO_NORMAL_ADDR = 34235;
export const KIND_VIDEO_SHORT_ADDR = 34236;

export const VIDEO_KINDS = [KIND_VIDEO_NORMAL, KIND_VIDEO_SHORT, KIND_VIDEO_NORMAL_ADDR, KIND_VIDEO_SHORT_ADDR];

/** A SHORT (vertical/reel) video kind - the kind itself is the authoritative orientation signal per NIP-71,
 * so a short with no imeta `dim` still renders portrait instead of defaulting to landscape. */
export const isShortVideo = (kind: number): boolean => kind === KIND_VIDEO_SHORT || kind === KIND_VIDEO_SHORT_ADDR;
