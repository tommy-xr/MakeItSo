// Email Routing hands each message for capture@ here; it goes to MIS's `POST /api/capture`.
// Only the default export: the main module's named exports would be taken as entrypoints.
import { handle, type Env } from "./capture.ts";

export default {
  email: (message, env) => handle(message, env),
} satisfies ExportedHandler<Env>;
