import { InterfaceDefinition, notificationType, requestType, type LinkRpcInterfaceSchema } from "@hediet/linkrpc";
import { z } from "zod";

const ScreenshotSnapshotSchema = z.object({
    dataBase64: z.string(),
    mediaType: z.string(),
});

const wireSchema: LinkRpcInterfaceSchema = JSON.parse("{\"id\":\"dev.dbgjs.browser-automation\",\"hash\":\"c8c355c3ca85f6b7\",\"methods\":{\"capture_screenshot\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ScreenshotSnapshot\"}},\"click_target\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"selector\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"selector\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"}},\"type_target\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\",\"text\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"},\"text\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"}}},\"components\":{\"schemas\":{\"ScreenshotSnapshot\":{\"type\":\"object\",\"required\":[\"dataBase64\",\"mediaType\"],\"properties\":{\"dataBase64\":{\"type\":\"string\"},\"mediaType\":{\"type\":\"string\"}},\"additionalProperties\":false}}}}");

export const BrowserAutomationApi = new InterfaceDefinition(
    {
        id: "dev.dbgjs.browser-automation",
        hash: "c8c355c3ca85f6b7",
    },
    {
        capture_screenshot: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                targetId: z.string(),
            }),
            ScreenshotSnapshotSchema,
        ),
        click_target: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                selector: z.string(),
                targetId: z.string(),
            }),
            z.boolean(),
        ),
        type_target: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                targetId: z.string(),
                text: z.string(),
            }),
            z.boolean(),
        ),
    },
    { frozenSchema: wireSchema },
);
