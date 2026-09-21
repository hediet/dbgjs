import { InterfaceDefinition, notificationType, requestType, type LinkRpcInterfaceSchema } from "@hediet/linkrpc";
import { z } from "zod";

const wireSchema: LinkRpcInterfaceSchema = JSON.parse("{\"id\":\"dev.dbgjs.cdp-access\",\"hash\":\"2dcbee21e3e77b09\",\"methods\":{\"raw_cdp_request\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"method\",\"params\",\"targetId\",\"validate\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"method\":{\"type\":\"string\"},\"params\":true,\"targetId\":{\"type\":\"string\"},\"validate\":{\"type\":\"boolean\"}},\"additionalProperties\":false},\"result\":true},\"raw_cdp_session_request\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"method\",\"params\",\"sessionId\",\"targetId\",\"validate\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"method\":{\"type\":\"string\"},\"params\":true,\"sessionId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"},\"validate\":{\"type\":\"boolean\"}},\"additionalProperties\":false},\"result\":true}}}");

export const CdpAccessApi = new InterfaceDefinition(
    {
        id: "dev.dbgjs.cdp-access",
        hash: "2dcbee21e3e77b09",
    },
    {
        raw_cdp_request: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                method: z.string(),
                params: z.unknown(),
                targetId: z.string(),
                validate: z.boolean(),
            }),
            z.unknown(),
        ),
        raw_cdp_session_request: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                method: z.string(),
                params: z.unknown(),
                sessionId: z.string(),
                targetId: z.string(),
                validate: z.boolean(),
            }),
            z.unknown(),
        ),
    },
    { frozenSchema: wireSchema },
);
