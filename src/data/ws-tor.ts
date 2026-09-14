// Route .onion relay WebSockets through a Tor SOCKS5 proxy. Set TOR_SOCKS (e.g.
// `socks5h://tor:9050` - the compose `tor` service); `socks5h` makes the proxy do
// the (.onion) name resolution, which a SOCKS client can't. Clearnet relays connect
// directly (no proxy). With TOR_SOCKS unset this is a no-op and .onion relays simply
// fail to connect (SimplePool tolerates it) - the prior behaviour.
//
// Mechanism: nostr-tools' `useWebSocketImplementation` lets us swap in a WebSocket
// class. Ours subclasses `ws` and attaches a SOCKS agent only for .onion hosts, so
// one impl serves both clearnet (direct) and onion (tunnelled) relays.

import { relaysViaTor } from '../privacy.ts';
import { isLocalRelayUrl } from '../local-relay.ts';

const isOnion = (address: string): boolean => {
    try { return new URL(address).hostname.toLowerCase().endsWith('.onion'); } catch { return false; }
};

// A relay goes through Tor when it's .onion (the only way it connects) OR Privacy Mode routes
// clearnet relays too - EXCEPT the configured local relay, which bypasses the Privacy-Mode forcing:
// a clearnet-local relay (ws://localhost) isn't reachable over Tor, so forcing it would just break
// it. (An .onion local relay still routes via Tor - isOnion catches it first.) Read LIVE per-
// connection, so a Settings toggle takes effect on the next relay dial without a restart.
const routeViaTor = (address: string): boolean =>
    isOnion(address) || (relaysViaTor() && !isLocalRelayUrl(address));

/** Install Tor routing for .onion relays if TOR_SOCKS is set. Call once at startup,
 * before any relay connects. Safe to call when unset (no-op). */
export async function installTorRouting(): Promise<void> {
    const proxy = process.env.TOR_SOCKS?.trim();
    if (!proxy) return;
    try {
        const [{ WebSocket: WS }, { SocksProxyAgent }, { useWebSocketImplementation }] = await Promise.all([
            import('ws'),
            import('socks-proxy-agent'),
            import('nostr-tools/pool'),
        ]);
        // One agent per relay host, each with a distinct SOCKS username → Tor isolates
        // them onto separate circuits, so a relay can't be correlated with the others.
        const agentByHost = new Map<string, InstanceType<typeof SocksProxyAgent>>();
        const agentFor = (address: string): InstanceType<typeof SocksProxyAgent> => {
            let host = '_';
            try { host = new URL(address).hostname; } catch { /* keep _ */ }
            let a = agentByHost.get(host);
            if (!a) { const purl = new URL(proxy); purl.username = `r-${host}`; purl.password = 'x'; a = new SocksProxyAgent(purl.href); agentByHost.set(host, a); }
            return a;
        };
        class TorWebSocket extends WS {
            constructor(address: string | URL, protocols?: string | string[]) {
                super(address, protocols, routeViaTor(String(address)) ? { agent: agentFor(String(address)) } : {});
            }
        }
        useWebSocketImplementation(TorWebSocket);
        console.log(`[tor] available via ${proxy} (.onion always; clearnet relays follow Privacy Mode)`);
    } catch (err) {
        // A missing dep or bad proxy URL must not take down the daemon - clearnet
        // relays still work; onion ones just won't connect.
        console.warn(`[tor] disabled (${err instanceof Error ? err.message : String(err)})`);
    }
}
