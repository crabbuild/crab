import { useState } from "react";
import { useSubmission } from "@/components/discussion/hooks";
import { useMutation } from "@/components/discussion/mutations";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { useRequest } from "@/lib/api";
import type { ActorProfile, OwnProfile } from "./model";

export function ProfileForm({
  profile,
  csrf,
  suggestedName,
  onSaved,
}: {
  profile: ActorProfile | null;
  csrf: string;
  suggestedName: string;
  onSaved: (value: ActorProfile) => void;
}) {
  const [handle, setHandle] = useState(profile?.handle ?? "");
  const [name, setName] = useState(profile?.name ?? suggestedName);
  const [version, setVersion] = useState(profile?.version ?? 0);
  const [notice, setNotice] = useState("");
  const mutation = useMutation(csrf, null);
  const submission = useSubmission();
  const latest = useRequest<OwnProfile>(
    mutation.conflict ? "/api/profile" : null,
  );
  const accept = (value: ActorProfile) => {
    setVersion(value.version);
    setHandle(value.handle);
    setName(value.name);
    onSaved(value);
  };
  return (
    <form
      className="min-w-0 space-y-6"
      onSubmit={async (event) => {
        event.preventDefault();
        setNotice("");
        const fields = { handle, name };
        const input = version
          ? { ...fields, version }
          : { ...fields, request_id: submission(fields) };
        const method = version ? "PATCH" : "POST";
        const result = await mutation.run<ActorProfile>(
          "/api/profile",
          method,
          input,
        );
        if (!result) return;
        accept(result);
        setNotice(version ? "Profile updated." : "Profile created.");
      }}
    >
      <fieldset
        className="m-0 grid min-w-0 gap-6 border-0 p-0"
        disabled={mutation.pending}
      >
        <legend className="sr-only">Profile details</legend>
        <div className="grid gap-2">
          <Label htmlFor="profile-handle">Username</Label>
          <Input
            id="profile-handle"
            name="username"
            autoComplete="username"
            value={handle}
            onChange={(event) => {
              setHandle(event.target.value.toLowerCase());
              setNotice("");
            }}
            required
            maxLength={39}
            pattern="[a-z0-9]+(?:-[a-z0-9]+)*"
            aria-describedby="profile-handle-help"
          />
          <p
            id="profile-handle-help"
            className="m-0 text-xs text-muted-foreground"
          >
            Use 1–39 lowercase letters, digits, or single hyphens.
          </p>
        </div>
        <div className="grid gap-2">
          <Label htmlFor="profile-name">Display name</Label>
          <Input
            id="profile-name"
            name="name"
            autoComplete="name"
            value={name}
            onChange={(event) => {
              setName(event.target.value);
              setNotice("");
            }}
            required
            maxLength={200}
            aria-describedby="profile-name-help"
          />
          <p
            id="profile-name-help"
            className="m-0 text-xs text-muted-foreground"
          >
            Your name can contain up to 100 characters.
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-3">
          <Button type="submit">
            {mutation.pending
              ? "Saving…"
              : version
                ? "Update profile"
                : "Create profile"}
          </Button>
          {profile && <Link href={`/-/users/${profile.id}`}>View profile</Link>}
        </div>
      </fieldset>
      {notice && (
        <p role="status" className="notice">
          {notice}
        </p>
      )}
      {mutation.error && (
        <div role="alert" className="notice error">
          <p>{mutation.error}</p>
          <Button
            type="button"
            variant="outline"
            onClick={() => window.location.reload()}
          >
            Reload profile
          </Button>
        </div>
      )}
      {mutation.conflict && (
        <Result state={latest} timingLabel="Saved profile">
          {({ profile: saved }) =>
            saved && (
              <section
                className="notice space-y-3"
                aria-label="Review saved profile"
              >
                <h2 className="m-0 text-sm font-semibold">
                  Review the saved profile
                </h2>
                <p>
                  {saved.name} · @{saved.handle}
                </p>
                <div className="flex flex-wrap gap-2">
                  <Button
                    type="button"
                    variant="outline"
                    onClick={() => {
                      accept(saved);
                      mutation.reset();
                    }}
                  >
                    Use saved profile
                  </Button>
                  <Button
                    type="button"
                    variant="outline"
                    onClick={() => {
                      setVersion(saved.version);
                      mutation.reset();
                    }}
                  >
                    Keep my draft
                  </Button>
                </div>
              </section>
            )
          }
        </Result>
      )}
    </form>
  );
}
