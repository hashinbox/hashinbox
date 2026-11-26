// Lets `node --test` run the sources as they are: their imports name no
// extension (`./client`, as bundlers and tsc resolve them), so a relative
// one that does not resolve is tried with `.ts`.
import { registerHooks } from "node:module";

registerHooks({
  resolve(specifier, context, next) {
    try {
      return next(specifier, context);
    } catch (error) {
      if (!specifier.startsWith(".")) throw error;
      return next(`${specifier}.ts`, context);
    }
  },
});
