export type StorageCredentials =
  | { provider: "s3"; access_key_id: string; secret_access_key: string; session_token?: string }
  | { provider: "gcs"; service_account_key: string }
  | { provider: "azure"; account: string; access_key?: string; sas_token?: string };

export interface StorageConnection {
  id: string;
  name: string;
  provider: StorageCredentials["provider"];
  bucket: string;
  region: string;
  endpoint: string;
  credentials: StorageCredentials;
}

export type StorageConnectionInfo = Omit<StorageConnection, "credentials">;
export type StorageConnectionInput = Omit<StorageConnection, "id"> & { id?: string };

export interface StorageConnectionStatus {
  connections: StorageConnectionInfo[];
  selected_id: string | null;
}
