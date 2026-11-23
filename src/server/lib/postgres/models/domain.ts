/**
 * Domains as resources.
 *
 * - `domains` holds every domain this server receives mail for, one row per
 *   name. Its `user_id` is the domain's one owner, and also the account whose
 *   `user_id` keys the domain's mail rows: a domain's mail is stored with its
 *   owner, so a domain never moves rows when someone is granted a role on it.
 * - `domain_claims` holds a name someone has asked to add and not yet proved
 *   with its DNS record. A claim is per person, so a claim nobody can verify
 *   blocks no one: the row moves into `domains` only once the record resolves.
 * - `former_domains` remembers the domains an account owned and no longer
 *   does (removed, or verified away by someone else). Their mail stays in the
 *   account's rows, so its account list keeps showing their addresses.
 * - `domain_roles` holds the people other than the owner who have a role on a
 *   domain. The one role stored here today is `viewer`; the owner is the
 *   domain row's `user_id`, which is what keeps a domain to exactly one owner.
 */

import { Model, createTable } from "./base";
import { USER_ID } from "./common";

export const DOMAINS = "domains";
export const DOMAIN_CLAIMS = "domain_claims";
export const DOMAIN_ROLES = "domain_roles";
export const FORMER_DOMAINS = "former_domains";

export const DOMAIN_ID = "domain_id";
export const DOMAIN_NAME = "name";
export const DOMAIN_VERIFIED_AT = "verified_at";
export const DOMAIN_CREATED_AT = "created_at";
export const CLAIM_ID = "claim_id";
export const CLAIM_TOKEN = "token";
export const ROLE = "role";

/** The longest DNS name, RFC 1035 §2.3.4. */
export const DOMAIN_NAME_MAX_LENGTH = 253;

/** Ceiling on the domains and pending claims one person holds together. */
export const DOMAIN_COUNT_MAX = 100;

/** Ceiling on the viewers of one domain. */
export const DOMAIN_VIEWER_COUNT_MAX = 100;

export type DomainRole = "owner" | "viewer";

const isString = (v: unknown): v is string => typeof v === "string";
const isNullableString = (v: unknown): v is string | null =>
  v === null || typeof v === "string";

export interface DomainJSON {
  domain_id: string;
  name: string;
  user_id: string;
  verified_at: string | null;
  created_at: string;
}

const domainSchema = {
  [DOMAIN_ID]: "UUID PRIMARY KEY DEFAULT gen_random_uuid()",
  [DOMAIN_NAME]: `VARCHAR(${DOMAIN_NAME_MAX_LENGTH}) UNIQUE NOT NULL`,
  [USER_ID]: "UUID NOT NULL REFERENCES users(user_id) ON DELETE CASCADE",
  // When the DNS record was last proved; null for a domain this server
  // assigned itself (the accounts' own names under EMAIL_DOMAIN, or one the
  // operator gave an account with the user script).
  [DOMAIN_VERIFIED_AT]: "TIMESTAMPTZ",
  [DOMAIN_CREATED_AT]: "TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP",
};

type DomainSchema = typeof domainSchema;

export class DomainModel extends Model<DomainJSON, DomainSchema> {
  declare domain_id: string;
  declare name: string;
  declare user_id: string;
  declare verified_at: string | null;
  declare created_at: string;

  static typeChecker = {
    domain_id: isString,
    name: isString,
    user_id: isString,
    verified_at: isNullableString,
    created_at: isString,
  };

  constructor(data: unknown) {
    super(data, DomainModel.typeChecker);
  }

  toJSON(): DomainJSON {
    return {
      domain_id: this.domain_id,
      name: this.name,
      user_id: this.user_id,
      verified_at: this.verified_at,
      created_at: this.created_at,
    };
  }
}

export const domainsTable = createTable({
  name: DOMAINS,
  primaryKey: DOMAIN_ID,
  schema: domainSchema,
  indexes: [{ column: USER_ID }],
  ModelClass: DomainModel,
  supportsSoftDelete: false,
});

export interface DomainClaimJSON {
  claim_id: string;
  name: string;
  user_id: string;
  token: string;
  created_at: string;
}

const domainClaimSchema = {
  [CLAIM_ID]: "UUID PRIMARY KEY DEFAULT gen_random_uuid()",
  [DOMAIN_NAME]: `VARCHAR(${DOMAIN_NAME_MAX_LENGTH}) NOT NULL`,
  [USER_ID]: "UUID NOT NULL REFERENCES users(user_id) ON DELETE CASCADE",
  [CLAIM_TOKEN]: "VARCHAR(64) NOT NULL",
  [DOMAIN_CREATED_AT]: "TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP",
};

type DomainClaimSchema = typeof domainClaimSchema;

export class DomainClaimModel extends Model<DomainClaimJSON, DomainClaimSchema> {
  declare claim_id: string;
  declare name: string;
  declare user_id: string;
  declare token: string;
  declare created_at: string;

  static typeChecker = {
    claim_id: isString,
    name: isString,
    user_id: isString,
    token: isString,
    created_at: isString,
  };

  constructor(data: unknown) {
    super(data, DomainClaimModel.typeChecker);
  }

  toJSON(): DomainClaimJSON {
    return {
      claim_id: this.claim_id,
      name: this.name,
      user_id: this.user_id,
      token: this.token,
      created_at: this.created_at,
    };
  }
}

export const domainClaimsTable = createTable({
  name: DOMAIN_CLAIMS,
  primaryKey: CLAIM_ID,
  schema: domainClaimSchema,
  constraints: [`UNIQUE (${DOMAIN_NAME}, ${USER_ID})`],
  indexes: [{ column: USER_ID }],
  ModelClass: DomainClaimModel,
  supportsSoftDelete: false,
});

export interface DomainRoleJSON {
  domain_id: string;
  user_id: string;
  role: string;
  created_at: string;
}

const domainRoleSchema = {
  [DOMAIN_ID]: "UUID NOT NULL REFERENCES domains(domain_id) ON DELETE CASCADE",
  [USER_ID]: "UUID NOT NULL REFERENCES users(user_id) ON DELETE CASCADE",
  [ROLE]: "VARCHAR(16) NOT NULL CHECK (role IN ('viewer'))",
  [DOMAIN_CREATED_AT]: "TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP",
};

type DomainRoleSchema = typeof domainRoleSchema;

export class DomainRoleModel extends Model<DomainRoleJSON, DomainRoleSchema> {
  declare domain_id: string;
  declare user_id: string;
  declare role: string;
  declare created_at: string;

  static typeChecker = {
    domain_id: isString,
    user_id: isString,
    role: isString,
    created_at: isString,
  };

  constructor(data: unknown) {
    super(data, DomainRoleModel.typeChecker);
  }

  toJSON(): DomainRoleJSON {
    return {
      domain_id: this.domain_id,
      user_id: this.user_id,
      role: this.role,
      created_at: this.created_at,
    };
  }
}

export const domainRolesTable = createTable({
  name: DOMAIN_ROLES,
  primaryKey: DOMAIN_ID,
  schema: domainRoleSchema,
  constraints: [`PRIMARY KEY (${DOMAIN_ID}, ${USER_ID})`],
  indexes: [{ column: USER_ID }],
  ModelClass: DomainRoleModel,
  supportsSoftDelete: false,
});

export interface FormerDomainJSON {
  user_id: string;
  name: string;
  created_at: string;
}

const formerDomainSchema = {
  [USER_ID]: "UUID NOT NULL REFERENCES users(user_id) ON DELETE CASCADE",
  [DOMAIN_NAME]: `VARCHAR(${DOMAIN_NAME_MAX_LENGTH}) NOT NULL`,
  [DOMAIN_CREATED_AT]: "TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP",
};

type FormerDomainSchema = typeof formerDomainSchema;

export class FormerDomainModel extends Model<FormerDomainJSON, FormerDomainSchema> {
  declare user_id: string;
  declare name: string;
  declare created_at: string;

  static typeChecker = {
    user_id: isString,
    name: isString,
    created_at: isString,
  };

  constructor(data: unknown) {
    super(data, FormerDomainModel.typeChecker);
  }

  toJSON(): FormerDomainJSON {
    return { user_id: this.user_id, name: this.name, created_at: this.created_at };
  }
}

export const formerDomainsTable = createTable({
  name: FORMER_DOMAINS,
  primaryKey: USER_ID,
  schema: formerDomainSchema,
  constraints: [`PRIMARY KEY (${USER_ID}, ${DOMAIN_NAME})`],
  ModelClass: FormerDomainModel,
  supportsSoftDelete: false,
});
