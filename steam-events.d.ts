// Shapes returned by runtime / lobby / sockets. All 64-bit IDs are decimal strings.
// `steamUsec` on events is the SteamNetworkingUtils local clock at queue time.

/** ESteamNetworkingConnectionState: 0 none, 1 connecting, 2 finding route, 3 connected, 4 closed by peer, 5 problem detected locally. */
export type ConnectionState = number

export interface ConnectionInfo {
    connection: number
    peer: string
    identityType: number
    listenSocket: number
    userData: number
    state: ConnectionState
    endReason: number
    endDebug: string
    description: string
    flags: number
    unauthenticated: boolean
    unencrypted: boolean
    relayed: boolean
    popRelay: number
    popRemote: number
}

export interface RealtimeStatus {
    state: ConnectionState
    ping: number
    qualityLocal: number
    qualityRemote: number
    outPacketsPerSec: number
    outBytesPerSec: number
    inPacketsPerSec: number
    inBytesPerSec: number
    sendRateBytesPerSecond: number
    pendingUnreliable: number
    pendingReliable: number
    sentUnackedReliable: number
    queueTimeUsec: number
    maxJitterUsec: number
}

export interface RelayStatus {
    availability: number
    pingMeasurementInProgress: boolean
    availNetworkConfig: number
    availAnyRelay: number
    debug: string
}

export interface LobbySummary {
    id: string
    memberCount: number
    memberLimit: number
    data: Record<string, string>
}

export interface LobbySnapshot extends LobbySummary {
    owner: string
    members: Array<{ id: string; name: string; data: Record<string, string> }>
}

export interface Identity {
    steamId: string
    name: string
    appId: number
    loggedOn: boolean
    interfaces: { matchmaking: string; networkingSockets: string; networkingUtils: string }
}

export interface PumpStats {
    runs: number
    gaps: number
    meanGapUs: number
    maxGapUs: number
    lastGapUs: number
    maxRunUs: number
    queuedEvents: number
    droppedEvents: number
}

export interface SocketStats {
    listenSockets: number
    connections: number
    pollGroup: number
    receivedMessages: number
    releasedMessages: number
    receivedBytes: number
}

interface Timed {
    steamUsec: number
}

export type SteamEvent = Timed &
    (
        | { type: 'lobby-create-result'; token: number; ok: boolean; lobby: string | null; result: string }
        | { type: 'lobby-join-result'; token: number; ok: boolean; lobby: string }
        | { type: 'lobby-list-result'; token: number; ok: boolean; result: string; lobbies: LobbySummary[] }
        /** Raw EChatRoomEnterResponse: 1 success, 2 doesn't exist, 3 not allowed, 4 full, 5 error, ... */
        | { type: 'lobby-enter'; lobby: string; response: number; locked: boolean }
        | { type: 'lobby-created'; lobby: string; result: string }
        /** `change` is an EChatMemberStateChange bitmask: 1 entered, 2 left, 4 disconnected, 8 kicked, 16 banned. */
        | { type: 'lobby-chat-update'; lobby: string; userChanged: string; makingChange: string; change: number }
        /** `member` equals `lobby` for lobby data, otherwise the member whose data changed. */
        | { type: 'lobby-data-update'; lobby: string; member: string; success: boolean }
        | { type: 'lobby-chat-msg'; lobby: string; sender: string; entrySender: string; entryType: number; chatId: number; text: string }
        | ({ type: 'connection-status'; oldState: ConnectionState } & ConnectionInfo)
        | { type: 'authentication-status'; availability: number; debug: string }
        | ({ type: 'relay-status' } & RelayStatus)
        | { type: 'steam-servers-connected' }
        | { type: 'steam-servers-disconnected'; reason: string }
        | { type: 'steam-server-connect-failure'; reason: string; stillRetrying: boolean }
    )
