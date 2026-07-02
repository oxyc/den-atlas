# den-atlas — multi-stage: compile TS → run the plain-JS output as a non-root Node process.
# No native deps (all I/O is fs + the built-in HTTP server), so the runtime image is node:slim + dist +
# prod deps + the dataset blobs. Run `npm run import` BEFORE building so ./data holds the artifacts.

FROM node:22-bookworm-slim AS build
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci
COPY tsconfig.json tsconfig.build.json ./
COPY src ./src
RUN npm run build && npm prune --omit=dev

FROM node:22-bookworm-slim AS runtime
ENV NODE_ENV=production \
    PORT=8080 \
    ATLAS_DATA_DIR=/app/data
WORKDIR /app
COPY --from=build /app/node_modules ./node_modules
COPY --from=build /app/dist ./dist
COPY package.json ./
# The derived dataset blobs (gitignored; produced by `npm run import`). ~33 MB.
COPY data ./data

# node:*-slim ships an unprivileged `node` user (uid 1000) — run as it, never root.
USER node
EXPOSE 8080

# curl isn't in the slim image; Node's global fetch does the healthcheck.
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
  CMD node -e "fetch('http://127.0.0.1:'+(process.env.PORT||8080)+'/health').then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))"

CMD ["node", "dist/server.js"]
