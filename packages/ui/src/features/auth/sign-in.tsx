import { CircleAlertIcon, LogInIcon, UserRoundPlusIcon } from "lucide-react";
import { MarkGithubIcon } from "@primer/octicons-react";
import { Button } from "@/components/ui/button";
import { Link } from "@/components/navigation/link";
import { Card, CardContent, CardHeader } from "@/components/ui/card";
import type { Session } from "@/lib/contracts";
import crabMark from "../../../../web/crab.optimized.svg";
import "./sign-in.css";

export function SignIn({
  mode,
  canSignUp,
  url,
}: {
  mode: Session["mode"];
  canSignUp: boolean;
  url: URL;
}) {
  const github = mode === "github";
  const failed = url.searchParams.has("auth_error");
  const login = signInHref(url);
  const signUp = `${login}&sign_up=true`;

  return (
    <section className="sign-in" aria-labelledby="sign-in-title">
      <div className="sign-in-logo" aria-hidden="true">
        <img className="brand-mark" src={crabMark} alt="" />
      </div>
      <Card className="sign-in-content">
        <CardHeader className="sign-in-header">
          <h1 id="sign-in-title">Sign in to Crab</h1>
        </CardHeader>
        <CardContent>
          {failed && (
            <p className="sign-in-error" role="alert">
              <CircleAlertIcon aria-hidden="true" />
              <span>
                We couldn’t sign you in. Try again or contact your
                administrator.
              </span>
            </p>
          )}
          <div className="sign-in-actions">
            <Button asChild className="sign-in-action">
              <a href={login}>
                {github ? (
                  <MarkGithubIcon aria-hidden="true" />
                ) : (
                  <LogInIcon aria-hidden="true" />
                )}
                {github ? "Continue with GitHub" : "Continue to sign in"}
              </a>
            </Button>
            {github || canSignUp ? (
              <Button asChild variant="outline" className="sign-in-action">
                <a href={github ? "https://github.com/signup" : signUp}>
                  <UserRoundPlusIcon aria-hidden="true" />
                  {github ? "Sign up with GitHub" : "Sign up"}
                </a>
              </Button>
            ) : null}
            <Button asChild variant="ghost" className="sign-in-action">
              <Link href="/-/public">Browse public repositories</Link>
            </Button>
          </div>
          {!github && !canSignUp && (
            <p className="sign-in-help">
              If your provider offers registration, you can sign up on its
              sign-in page.
            </p>
          )}
        </CardContent>
      </Card>
    </section>
  );
}

export function signInHref(url: URL) {
  const query = new URLSearchParams(url.search);
  query.delete("auth_error");
  const search = query.toString();
  const returnTo = `${url.pathname}${search ? `?${search}` : ""}${url.hash}`;
  return `/auth/login?${new URLSearchParams({ return_to: returnTo })}`;
}
