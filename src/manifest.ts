/**
 * Den Atlas Stremio-superset manifest. It declares the Den-specific `dataset` resource (FP-1 in the Den
 * repo — `EPIC-feature-provider-addon`), which the app's `AddonClient` routes on. A plain Stremio client
 * ignores an unknown resource, so installing Atlas there is harmless; only Den acts on it.
 *
 * No per-user config (the dataset is public, non-personal, ToS-clean derived data), so — unlike scout —
 * there is no token, no `/configure`, and `configurationRequired` is false.
 */
const VERSION = "0.1.0";

export function buildManifest(): Record<string, unknown> {
  return {
    id: "com.den.atlas",
    version: VERSION,
    name: "Den Atlas",
    description:
      "A map of the catalog: derived labels (genre / subgenre / mood) + semantic vectors the Den app " +
      "downloads and refreshes for similar-titles, categories, and billboard. Derived data only.",
    // The Den superset resource. `catalog` MAY be added later (FP-2) for curated billboard rows.
    resources: ["dataset"],
    types: ["movie", "series"],
    idPrefixes: ["tt"],
    catalogs: [],
    behaviorHints: {
      configurable: false,
      configurationRequired: false,
    },
  };
}
