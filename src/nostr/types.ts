// Shared nostr types. Structurally compatible with nostr-tools' Event, so values
// flow between the two without conversion - but defined here to keep the pure
// nostr/ helpers independent of any library.

export interface NostrEvent {
    id: string;
    pubkey: string;
    created_at: number;
    kind: number;
    tags: string[][];
    content: string;
    sig: string;
}

/** An event template before the signer fills in id/sig. */
export interface UnsignedEvent {
    kind: number;
    created_at: number;
    tags: string[][];
    content: string;
    pubkey: string;
}

/** A pubkey's NIP-65 relays: where they read (inbox) and write (outbox). */
export interface RelayList {
    read: string[];
    write: string[];
}

/** One editable relay row (Settings). */
export interface RelayEntry {
    url: string;
    read: boolean;
    write: boolean;
}
