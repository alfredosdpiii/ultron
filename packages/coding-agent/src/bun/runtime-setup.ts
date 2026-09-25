import { bedrockProviderModule } from "@ultron/ai/bedrock-provider";
import { registerBunOAuthFlows } from "@ultron/ai/bun-oauth";
import { setBedrockProviderModule } from "@ultron/ai/compat";
import { APP_NAME } from "../config.ts";

process.title = APP_NAME;
process.emitWarning = (() => {}) as typeof process.emitWarning;
registerBunOAuthFlows();
setBedrockProviderModule(bedrockProviderModule);
