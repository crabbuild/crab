# ECS Fargate deployment profile

This CloudFormation template is the checked-in ECS deployment asset for
`crab-s3-gateway`. It is a deployment profile, not live AWS qualification.
The stack creates an ECR repository, task/execution roles, retained CloudWatch
logs, an internal HTTPS Application Load Balancer, and a two-task Fargate
service. It deliberately requires the caller to provide an existing ECS
cluster, VPC/subnets, security groups, ACM certificate, Crab backend bucket and
Secrets Manager values; it never creates a second repository placement.

The task uses an immutable `sha256:` image digest, a read-only root filesystem,
separate bounded scratch/cache volumes, a 45-second target deregistration
delay, and a 100% minimum healthy deployment. ECS retrieves the two secret
values with the execution role. The task's entrypoint writes them as protected
files in the disposable scratch volume before starting the gateway:

- `ConfigSecretArn` contains the complete validated TOML configuration. Its
  `secret_key_file` must be `/var/lib/crab/tmp/client-secret`, and its
  `[cache].directory` must be `/var/lib/crab/cache-volume/cache`.
- `GatewayCredentialSecretArn` contains the client S3 credential secret only.
  Backend object-store access uses the task role's container credential chain;
  backend keys must not be placed in either secret.

The existing security groups must allow the selected client sources to the ALB
on 443, ALB-to-task traffic on 8080, and task egress to the backend, ECR,
Secrets Manager, CloudWatch Logs, and the configured KMS keys. The management
port 8081 is not exposed by the load balancer. The ALB terminates TLS and
forwards the original S3 path and query to the gateway; configure the
certificate and DNS name for the client hostname used in SigV4 signing.

## Validate and deploy

Use a clean checkout and the same image digest that passed the packaged-image
qualification. The parameter example contains placeholders only:

```sh
aws cloudformation validate-template \
  --template-body file://crates/crab-s3-gateway/deploy/ecs/crab-s3-gateway.yaml

aws cloudformation deploy \
  --template-file crates/crab-s3-gateway/deploy/ecs/crab-s3-gateway.yaml \
  --stack-name crab-s3-gateway \
  --capabilities CAPABILITY_NAMED_IAM \
  --parameter-overrides file://crates/crab-s3-gateway/deploy/ecs/parameters.example.json
```

Before deployment, push the qualified image to the output ECR repository and
replace `ImageDigest` with the complete digest. Confirm that the task CPU and
memory are a valid Fargate combination, that service and load-balancer subnets
span at least two availability zones, and that the gateway TOML names the
existing Crab bucket/prefix selected by `BackendBucketName` and
`BackendPrefix`. Keep the backend bucket and prefix outside this stack's
deletion scope.

Inspect the rollout and endpoint without exposing secret values:

```sh
aws ecs describe-services --cluster CLUSTER_ARN --services crab-s3-gateway
aws ecs wait services-stable --cluster CLUSTER_ARN --services crab-s3-gateway
aws cloudformation describe-stacks --stack-name crab-s3-gateway \
  --query 'Stacks[0].Outputs'
aws logs tail /ecs/crab-s3-gateway --since 10m
```

Run the unchanged AWS CLI or SDK through the output HTTPS DNS name. Required
qualification still includes signed discovery, PUT/GET/range, multipart
completion, forced task replacement, active-transfer rolling upgrade,
credential rotation/revocation, and safe teardown. Those checks require a
dedicated AWS account and are not claimed by static template validation.

## Replacement and teardown

The ECS deployment circuit breaker rolls back an unhealthy revision while
retaining the previous task set. Stop one task only after another task is
healthy, then verify that durable upload state and completion receipts allow the
replacement to continue. Never use task-local scratch or cache as recovery
state.

Deleting the stack removes compute, load-balancer, IAM, and task-owned logging
resources according to their retention policies. The ECR repository is not
emptied automatically, and the existing ECS cluster, VPC, security groups,
Secrets Manager values, ACM certificate, backend bucket, and Crab repository
prefix remain caller-owned. Do not delete those resources as part of a gateway
incident or rollback.
