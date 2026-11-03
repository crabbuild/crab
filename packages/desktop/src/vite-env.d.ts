/// <reference types="vite/client" />

// Vite's `?worker` import pattern. Resolves to a constructor-like
// class that Monaco's worker factory can `new`.
declare module "*?worker" {
  const workerConstructor: {
    new (options?: { name?: string }): Worker;
  };
  export default workerConstructor;
}
