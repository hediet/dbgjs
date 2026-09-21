import { InterfaceDefinition, notificationType, requestType, type LinkRpcInterfaceSchema } from "@hediet/linkrpc";
import { z } from "zod";

const PlaywrightProxyEndpointSchema = z.object({
    connectionGeneration: z.int(),
    id: z.string(),
    websocketUrl: z.string(),
});

/**
 * A short-lived, authenticated loopback CDP endpoint exposed by `dbgjs context relay` or `dbgjs target relay`. `id` identifies the relay for `close_relay`; `websocket_url` carries its own random capability token and must not be reused once the relay closes.
 */
const RelayEndpointSchema = z.object({
    id: z.string(),
    websocketUrl: z.string(),
}).describe("A short-lived, authenticated loopback CDP endpoint exposed by `dbgjs context relay` or `dbgjs target relay`. `id` identifies the relay for `close_relay`; `websocket_url` carries its own random capability token and must not be reused once the relay closes.");

const wireSchema: LinkRpcInterfaceSchema = JSON.parse("{\"id\":\"dev.dbgjs.relay\",\"hash\":\"caed27d701c601ed\",\"methods\":{\"close_playwright_proxy\":{\"params\":{\"type\":\"object\",\"required\":[\"proxyId\"],\"properties\":{\"proxyId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"}},\"close_relay\":{\"params\":{\"type\":\"object\",\"required\":[\"relayId\"],\"properties\":{\"relayId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"},\"description\":\"Closes a relay opened by `open_context_relay` or `open_target_relay`, restoring ordinary\\nlocal access to its context. Returns `false` if the relay was already closed.\"},\"open_context_relay\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\"],\"properties\":{\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/RelayEndpoint\"},\"description\":\"Opens a virtual browser-root CDP relay exposing every target across every connection in\\n`context_id` as one endpoint. Takes exclusive relay ownership of the context immediately:\\nordinary local target debugging commands fail until the relay closes. Does not restart\\nany underlying connection; existing attachments and future ones stay lazy.\"},\"open_playwright_proxy\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"expectedGeneration\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"expectedGeneration\":{\"type\":\"integer\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/PlaywrightProxyEndpoint\"}},\"open_target_relay\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/RelayEndpoint\"},\"description\":\"Opens a direct-root CDP relay exposing exactly one target. Takes the same exclusive\\nrelay ownership of the target's owning context as `open_context_relay`.\"}},\"components\":{\"schemas\":{\"PlaywrightProxyEndpoint\":{\"type\":\"object\",\"required\":[\"connectionGeneration\",\"id\",\"websocketUrl\"],\"properties\":{\"connectionGeneration\":{\"type\":\"integer\"},\"id\":{\"type\":\"string\"},\"websocketUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"RelayEndpoint\":{\"description\":\"A short-lived, authenticated loopback CDP endpoint exposed by `dbgjs context relay` or `dbgjs target relay`. `id` identifies the relay for `close_relay`; `websocket_url` carries its own random capability token and must not be reused once the relay closes.\",\"type\":\"object\",\"required\":[\"id\",\"websocketUrl\"],\"properties\":{\"id\":{\"type\":\"string\"},\"websocketUrl\":{\"type\":\"string\"}},\"additionalProperties\":false}}}}");

export const RelayApi = new InterfaceDefinition(
    {
        id: "dev.dbgjs.relay",
        hash: "caed27d701c601ed",
    },
    {
        close_playwright_proxy: requestType(
            z.object({
                proxyId: z.string(),
            }),
            z.boolean(),
        ),
        /**
         * Closes a relay opened by `open_context_relay` or `open_target_relay`, restoring ordinary
         * local access to its context. Returns `false` if the relay was already closed.
         */
        close_relay: requestType(
            z.object({
                relayId: z.string(),
            }),
            z.boolean(),
            {
                description: "Closes a relay opened by `open_context_relay` or `open_target_relay`, restoring ordinary\nlocal access to its context. Returns `false` if the relay was already closed.",
            },
        ),
        /**
         * Opens a virtual browser-root CDP relay exposing every target across every connection in
         * `context_id` as one endpoint. Takes exclusive relay ownership of the context immediately:
         * ordinary local target debugging commands fail until the relay closes. Does not restart
         * any underlying connection; existing attachments and future ones stay lazy.
         */
        open_context_relay: requestType(
            z.object({
                contextId: z.string(),
            }),
            RelayEndpointSchema,
            {
                description: "Opens a virtual browser-root CDP relay exposing every target across every connection in\n`context_id` as one endpoint. Takes exclusive relay ownership of the context immediately:\nordinary local target debugging commands fail until the relay closes. Does not restart\nany underlying connection; existing attachments and future ones stay lazy.",
            },
        ),
        open_playwright_proxy: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                expectedGeneration: z.int(),
                targetId: z.string(),
            }),
            PlaywrightProxyEndpointSchema,
        ),
        /**
         * Opens a direct-root CDP relay exposing exactly one target. Takes the same exclusive
         * relay ownership of the target's owning context as `open_context_relay`.
         */
        open_target_relay: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                targetId: z.string(),
            }),
            RelayEndpointSchema,
            {
                description: "Opens a direct-root CDP relay exposing exactly one target. Takes the same exclusive\nrelay ownership of the target's owning context as `open_context_relay`.",
            },
        ),
    },
    { frozenSchema: wireSchema },
);
