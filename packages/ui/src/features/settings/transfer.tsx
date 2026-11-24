import { FieldSelect } from "@/components/ui/field-select";
import { useState, useRef, type FormEvent } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { useRequest, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import { refreshNotificationBadge } from "@/features/notifications/actions";

type Location = Pick<Repository, "owner" | "name">;
type Offer = {
  id: string;
  repository: string;
  from: Location;
  to: Location;
  recipient: string;
  state: "pending" | "accepted" | "declined" | "cancelled" | null;
  expires_at_ms: number;
  can_accept: boolean;
  can_cancel: boolean;
};

type Target =
  | { kind: "account"; handle: string }
  | { kind: "organization"; id: string };

type Proposal = {
  request_id: string;
  recipient: Target;
  name: string;
  version: number;
};

const status = {
  pending: "Awaiting acceptance",
  accepted: "Transfer accepted",
  declined: "Transfer declined",
  cancelled: "Transfer cancelled",
};
export const offerHref = (id: string) =>
  `/-/repository-transfers/${encodeURIComponent(id)}`;
const offerApi = (id: string) =>
  `/api/repository-transfers/${encodeURIComponent(id)}`;

export function TransferSettings({
  repo,
  csrf,
}: {
  repo: Repository;
  csrf: string;
}) {
  const [open, setOpen] = useState(false);
  if (!csrf || repo.archived) return null;
  return (
    <section
      className="settings-section"
      aria-labelledby="repository-transfer-heading"
    >
      <header>
        <div>
          <h3 id="repository-transfer-heading">Transfer ownership</h3>
          <p>Move this repository to another account or organization.</p>
        </div>
        <Dialog open={open} onOpenChange={setOpen}>
          <DialogTrigger asChild>
            <Button variant="outline" size="sm">
              Transfer repository
            </Button>
          </DialogTrigger>
          <DialogContent>
            <DialogHeader>
              <DialogTitle>Transfer repository</DialogTitle>
              <DialogDescription>
                The recipient must accept. Direct collaborators keep their
                access. Organization team and sign-in group grants are removed.
              </DialogDescription>
            </DialogHeader>
            {open && <TransferForm repo={repo} csrf={csrf} />}
          </DialogContent>
        </Dialog>
      </header>
    </section>
  );
}

function TransferForm({ repo, csrf }: { repo: Repository; csrf: string }) {
  const path = `/api/repositories/${repo.id}/transfer`;
  const settings = useRequest<{ version: number; offer: Offer | null }>(path);
  const mutation = useMutation(csrf, repo);
  const [kind, setKind] = useState<Target["kind"]>("account");
  const [value, setValue] = useState("");
  const [name, setName] = useState(repo.name);
  const [saved, setSaved] = useState<Offer>();
  const [copied, setCopied] = useState(false);
  const intent = useRef<{ key: string; payload: Proposal } | undefined>(
    undefined,
  );
  const validName =
    /^[a-z0-9._-]{1,100}$/i.test(name) && name !== "." && name !== "..";

  async function propose(event: FormEvent<HTMLFormElement>, version: number) {
    event.preventDefault();
    if (!value.trim() || !validName || mutation.pending) return;
    const entered = value.trim();
    const recipient: Target =
      kind === "account"
        ? { kind, handle: entered }
        : {
            kind,
            id: entered
              .replace(/^.*\/-\/organizations\//, "")
              .split(/[/?#]/, 1)[0],
          };
    const key = JSON.stringify({ recipient, name, version });
    if (intent.current?.key !== key) {
      intent.current = {
        key,
        payload: { request_id: crypto.randomUUID(), recipient, name, version },
      };
    }
    const result = await mutation.run<Offer>(
      path,
      "POST",
      intent.current.payload,
    );
    if (result) setSaved(result);
  }

  return (
    <Result state={settings} timingLabel="Repository transfer">
      {({ version, offer }) => {
        const current = saved ?? offer;
        return current?.state === "pending" ? (
          <div className="grid gap-3">
            <p role="status">Awaiting acceptance from {current.recipient}.</p>
            <p className="text-sm">
              The recipient can review this offer in Notifications for 24 hours.
              You can also share the link directly.
            </p>
            <Input
              aria-label="Transfer link"
              readOnly
              value={window.location.origin + offerHref(current.id)}
            />
            <div className="flex flex-wrap gap-2">
              <Button asChild variant="outline">
                <Link href={offerHref(current.id)}>View transfer</Link>
              </Button>
              <Button
                variant="outline"
                onClick={async () => {
                  try {
                    await navigator.clipboard.writeText(
                      window.location.origin + offerHref(current.id),
                    );
                    setCopied(true);
                  } catch {
                    setCopied(false);
                  }
                }}
              >
                {copied ? "Copied" : "Copy link"}
              </Button>
            </div>
          </div>
        ) : (
          <form
            className="grid gap-3"
            onSubmit={(event) => void propose(event, version)}
          >
            <label className="grid gap-1 text-sm font-medium">
              Recipient type
              <FieldSelect
                value={kind}
                onValueChange={(nextValue) => {
                  setKind(
                    nextValue === "organization" ? "organization" : "account",
                  );
                  setValue("");
                }}
                options={[
                  { value: "account", label: "Personal account" },
                  { value: "organization", label: "Organization" },
                ]}
              />
            </label>
            <label className="grid gap-1 text-sm font-medium">
              {kind === "account"
                ? "Recipient profile handle"
                : "Organization link or ID"}
              <Input
                value={value}
                onChange={(event) => setValue(event.target.value)}
                maxLength={256}
                required
              />
            </label>
            <label className="grid gap-1 text-sm font-medium">
              Repository name at destination
              <Input
                name="destination-name"
                value={name}
                onChange={(event) => setName(event.target.value)}
                maxLength={100}
                required
              />
            </label>
            {mutation.error && <p role="alert">{mutation.error}</p>}
            {mutation.status === 409 && (
              <Button
                type="button"
                variant="outline"
                onClick={() => {
                  setSaved(undefined);
                  mutation.reset();
                  settings.retry();
                }}
              >
                Reload transfer details
              </Button>
            )}
            <Button
              type="submit"
              className="justify-self-end"
              disabled={!value.trim() || !validName || mutation.pending}
            >
              {mutation.pending ? "Creating offer…" : "Create transfer offer"}
            </Button>
          </form>
        );
      }}
    </Result>
  );
}

export function TransferPage({ id, csrf }: { id: string; csrf: string }) {
  const path = offerApi(id);
  const request = useRequest<Offer>(path);
  const mutation = useMutation(csrf, null);
  const [confirmed, setConfirmed] = useState<Offer>();
  const [confirmation, setConfirmation] = useState("");
  return (
    <section
      className="mx-auto grid max-w-xl gap-4 px-4 py-8"
      aria-labelledby="transfer-title"
    >
      <h1 id="transfer-title" className="text-2xl font-semibold">
        Repository transfer
      </h1>
      <Result state={request} timingLabel="Repository transfer">
        {(loaded) => {
          const offer = confirmed ?? loaded;
          async function decide(decision: "accept" | "decline" | "cancel") {
            const result = await mutation.run<Offer>(path, "PUT", { decision });
            if (result) {
              setConfirmed(result);
              refreshNotificationBadge();
            }
          }
          return (
            <div className="grid gap-4 rounded-lg border p-5">
              <p role="status" className="font-medium">
                {offer.state
                  ? status[offer.state]
                  : "This offer is no longer available"}
              </p>
              <dl className="grid gap-2 text-sm">
                <div>
                  <dt className="text-muted-foreground">Current address</dt>
                  <dd className="break-all">
                    {offer.from.owner}/{offer.from.name}
                  </dd>
                </div>
                <div>
                  <dt className="text-muted-foreground">Destination address</dt>
                  <dd className="break-all">
                    {offer.to.owner}/{offer.to.name}
                  </dd>
                </div>
              </dl>
              {offer.can_accept && (
                <form
                  className="grid gap-3"
                  onSubmit={(event) => {
                    event.preventDefault();
                    if (confirmation === offer.to.name) void decide("accept");
                  }}
                >
                  <p className="text-sm">
                    Existing deploy tokens stay with the repository. You can
                    review and revoke them in Settings after accepting
                    ownership.
                  </p>
                  <label className="grid gap-1 text-sm">
                    Type <strong>{offer.to.name}</strong> to accept ownership.
                    <Input
                      name="confirm-transfer"
                      value={confirmation}
                      onChange={(event) => setConfirmation(event.target.value)}
                    />
                  </label>
                  <div className="flex flex-wrap gap-2">
                    <Button
                      type="submit"
                      disabled={
                        confirmation !== offer.to.name || mutation.pending
                      }
                    >
                      Accept transfer
                    </Button>
                    <Button
                      type="button"
                      variant="outline"
                      disabled={mutation.pending}
                      onClick={() => void decide("decline")}
                    >
                      Decline
                    </Button>
                  </div>
                </form>
              )}
              {offer.can_cancel && (
                <Button
                  variant="outline"
                  className="w-fit"
                  disabled={mutation.pending}
                  onClick={() => void decide("cancel")}
                >
                  Cancel transfer
                </Button>
              )}
              {offer.state === "accepted" && (
                <Button className="w-fit" asChild>
                  <Link
                    href={repoHref(offer.to, {
                      repository_id: offer.repository,
                    })}
                  >
                    Open repository
                  </Link>
                </Button>
              )}
              {mutation.error && <p role="alert">{mutation.error}</p>}
              {mutation.status === 409 && (
                <Button
                  variant="outline"
                  className="w-fit"
                  onClick={() => {
                    setConfirmed(undefined);
                    mutation.reset();
                    request.retry();
                  }}
                >
                  Reload transfer details
                </Button>
              )}
            </div>
          );
        }}
      </Result>
    </section>
  );
}
