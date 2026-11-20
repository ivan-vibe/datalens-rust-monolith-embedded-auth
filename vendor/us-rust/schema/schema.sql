--
-- PostgreSQL database dump
--


-- Dumped from database version 16.15 (Debian 16.15-1.pgdg13+2)
-- Dumped by pg_dump version 16.15 (Debian 16.15-1.pgdg13+2)

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

--
-- Name: btree_gin; Type: EXTENSION; Schema: -; Owner: -
--

CREATE EXTENSION IF NOT EXISTS btree_gin WITH SCHEMA public;


--
-- Name: EXTENSION btree_gin; Type: COMMENT; Schema: -; Owner: -
--

COMMENT ON EXTENSION btree_gin IS 'support for indexing common datatypes in GIN';


--
-- Name: btree_gist; Type: EXTENSION; Schema: -; Owner: -
--

CREATE EXTENSION IF NOT EXISTS btree_gist WITH SCHEMA public;


--
-- Name: EXTENSION btree_gist; Type: COMMENT; Schema: -; Owner: -
--

COMMENT ON EXTENSION btree_gist IS 'support for indexing common datatypes in GiST';


--
-- Name: pg_trgm; Type: EXTENSION; Schema: -; Owner: -
--

CREATE EXTENSION IF NOT EXISTS pg_trgm WITH SCHEMA public;


--
-- Name: EXTENSION pg_trgm; Type: COMMENT; Schema: -; Owner: -
--

COMMENT ON EXTENSION pg_trgm IS 'text similarity measurement and index searching based on trigrams';


--
-- Name: uuid-ossp; Type: EXTENSION; Schema: -; Owner: -
--

CREATE EXTENSION IF NOT EXISTS "uuid-ossp" WITH SCHEMA public;


--
-- Name: EXTENSION "uuid-ossp"; Type: COMMENT; Schema: -; Owner: -
--

COMMENT ON EXTENSION "uuid-ossp" IS 'generate universally unique identifiers (UUIDs)';


--
-- Name: comment_type; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.comment_type AS ENUM (
    'flag-x',
    'line-x',
    'band-x',
    'dot-x-y'
);


--
-- Name: data_export_status; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.data_export_status AS ENUM (
    'IN_PROGRESS',
    'CANCELLED',
    'FINISHED',
    'FAILED'
);


--
-- Name: embed_type_enum; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.embed_type_enum AS ENUM (
    'subscription'
);


--
-- Name: entity_bindings_target_type; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.entity_bindings_target_type AS ENUM (
    'entries',
    'workbooks'
);


--
-- Name: favorites_entity_type; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.favorites_entity_type AS ENUM (
    'entry',
    'workbook',
    'collection'
);


--
-- Name: license_limit_type_enum; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.license_limit_type_enum AS ENUM (
    'regular',
    'forced'
);


--
-- Name: license_type_enum; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.license_type_enum AS ENUM (
    'creator',
    'viewer'
);


--
-- Name: operation_status_enum; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.operation_status_enum AS ENUM (
    'scheduled',
    'failed',
    'done'
);


--
-- Name: scope; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.scope AS ENUM (
    'dataset',
    'pdf',
    'folder',
    'dash',
    'connection',
    'widget',
    'config',
    'report',
    'compute',
    'artifact'
);


--
-- Name: subscription_artifact_type_enum; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.subscription_artifact_type_enum AS ENUM (
    'png'
);


--
-- Name: subscription_content_type_enum; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.subscription_content_type_enum AS ENUM (
    'dash',
    'chart',
    'report'
);


--
-- Name: subscription_recipient_transport_type_enum; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.subscription_recipient_transport_type_enum AS ENUM (
    'email',
    'tg'
);


--
-- Name: subscription_status_enum; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.subscription_status_enum AS ENUM (
    'active',
    'stopped',
    'suspended'
);


--
-- Name: subscription_trigger_type_enum; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.subscription_trigger_type_enum AS ENUM (
    'cron',
    'dataset-refresh',
    'threshold',
    'relative',
    'non-empty',
    'is-true'
);


--
-- Name: workbook_status_enum; Type: TYPE; Schema: public; Owner: -
--

CREATE TYPE public.workbook_status_enum AS ENUM (
    'active',
    'creating',
    'deleting',
    'deleted'
);


--
-- Name: base36_encode(bigint, character[]); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.base36_encode(digits bigint, coding_base character[]) RETURNS character varying
    LANGUAGE plpgsql IMMUTABLE
    AS $$
    DECLARE
        ret varchar;
        val bigint;
    BEGIN
        val := digits;
        ret := '';
        IF val < 0 THEN
            val := val * - 1;
        END IF;
        WHILE val != 0 LOOP
            ret := coding_base [(val % 36)+1] || ret;
            val := val / 36;
        END LOOP;
        RETURN ret;
    END;
    $$;


--
-- Name: encode_id(bigint); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.encode_id(id bigint) RETURNS text
    LANGUAGE plpgsql IMMUTABLE
    AS $$
    DECLARE
        coding_base_str text;
        coding_base_sub1 text;
        coding_base_sub2 text;
        rotation_number int;
        coding_base_initial char[];
        coding_base_rotated char[];
        encoded_id text;
        last_symbol varchar;
    BEGIN
        -- Last two digits of original bigint id is a rotation_number
        -- We use it to shift encoding dictionary
        rotation_number := MOD(MOD(id, 100), 36);
        -- Splitting encoding dict into two parts in the exact spot
        -- (determined by rotation_number)
        coding_base_str := '0123456789abcdefghijklmnopqrstuvwxyz';
        coding_base_sub1 := substring(coding_base_str, rotation_number + 1);
        coding_base_sub2 := substring(coding_base_str, 0, rotation_number + 1);
        -- Converting original and rotated dictionaries into arrays
        coding_base_initial := regexp_split_to_array(coding_base_str, '');
        coding_base_rotated := regexp_split_to_array(concat(coding_base_sub1, coding_base_sub2), '');

        -- Resulting ID is a concatenation of original ID encoded by rotated dictionary
        -- and one char with with rotation_number encoded by original dictionary (so we can decode it later)
        last_symbol := base36_encode(rotation_number, coding_base_initial);

        IF last_symbol = ''
        THEN
            encoded_id := CONCAT(base36_encode(id, coding_base_rotated), '0');
        ELSE
            encoded_id := CONCAT(base36_encode(id, coding_base_rotated), last_symbol);
        END IF;
        return encoded_id;
    END;
    $$;


--
-- Name: get_id(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.get_id(OUT result bigint) RETURNS bigint
    LANGUAGE plpgsql
    AS $$
    DECLARE
        our_epoch bigint := 1514754000000;
        seq_id bigint;
        now_millis bigint;
        shard_id int := 1;
    BEGIN
        SELECT nextval('counter_seq') % 4096 INTO seq_id;

        SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000) INTO now_millis;
        result := (now_millis - our_epoch) << 23;
        result := result | (shard_id << 10);
        result := result | (seq_id);
    END;
    $$;


--
-- Name: naturalsort(text); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.naturalsort(text) RETURNS bytea
    LANGUAGE sql IMMUTABLE STRICT
    AS $_$
            SELECT string_agg(
                convert_to(
                    coalesce(
                        r[2],
                        length(length(r[1])::text) || length(r[1])::text || r[1]
                    ),
                    'UTF8'
                ),
                '\x00'
            ) from regexp_matches(
                regexp_replace(
                    regexp_replace($1, 'ё', 'е', 'g'),
                    '_', '!', 'g'
                ),
                '0*([0-9]+)|([^0-9]+)', 'g'
            ) r;
        $_$;


--
-- Name: update_collections(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.update_collections() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
            BEGIN
                NEW.sort_title := naturalsort(NEW.title);
                RETURN NEW;
            END
        $$;


--
-- Name: update_entries(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.update_entries() RETURNS trigger
    LANGUAGE plpgsql
    AS $_$
        DECLARE
          key TEXT;
        BEGIN
          key := NEW.key;

          NEW.name := SUBSTRING(key FROM '([^/]*)/?$');
          NEW.sort_name := naturalsort(NEW.name);

          RETURN NEW;
        END
        $_$;


--
-- Name: update_favorites(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.update_favorites() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
            BEGIN
                NEW.sort_alias := naturalsort(NEW.alias);
                IF NEW.entity_id IS NULL THEN
                    NEW.entity_id := NEW.entry_id;
                END IF;
                IF NEW.entity_type IS NULL THEN
                    NEW.entity_type := 'entry';
                END IF;
                RETURN NEW;
            END
        $$;


--
-- Name: update_workbooks(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.update_workbooks() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
            BEGIN
                NEW.sort_title := naturalsort(NEW.title);
                RETURN NEW;
            END
        $$;


SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: license_limits; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.license_limits (
    license_limit_id bigint DEFAULT public.get_id() NOT NULL,
    meta jsonb DEFAULT '{}'::jsonb NOT NULL,
    tenant_id text DEFAULT 'common'::text NOT NULL,
    type public.license_limit_type_enum NOT NULL,
    started_at timestamp with time zone NOT NULL,
    creators_limit_value integer NOT NULL,
    created_by text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_by text NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: license_quarantines; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.license_quarantines (
    license_quarantine_id bigint DEFAULT public.get_id() NOT NULL,
    license_type public.license_type_enum NOT NULL,
    started_at timestamp with time zone NOT NULL,
    ends_at timestamp with time zone NOT NULL,
    tenant_id text DEFAULT 'common'::text NOT NULL,
    meta jsonb DEFAULT '{}'::jsonb NOT NULL
);


--
-- Name: licenses; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.licenses (
    license_id bigint DEFAULT public.get_id() NOT NULL,
    meta jsonb DEFAULT '{}'::jsonb NOT NULL,
    tenant_id text DEFAULT 'common'::text NOT NULL,
    user_id text NOT NULL,
    license_type public.license_type_enum NOT NULL,
    expires_at timestamp with time zone,
    created_by text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_by text NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    quarantine_id bigint
);


--
-- Name: tenants; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.tenants (
    tenant_id text NOT NULL,
    meta jsonb DEFAULT '{}'::jsonb,
    created_at timestamp with time zone DEFAULT now(),
    enabled boolean DEFAULT false NOT NULL,
    deleting boolean DEFAULT false NOT NULL,
    last_init_at timestamp with time zone DEFAULT now() NOT NULL,
    retries_count integer DEFAULT 0 NOT NULL,
    collections_enabled boolean DEFAULT false NOT NULL,
    folders_enabled boolean DEFAULT true NOT NULL,
    billing_instance_service_id character varying(255) DEFAULT NULL::character varying,
    billing_started_at timestamp with time zone,
    branding jsonb DEFAULT '{}'::jsonb NOT NULL,
    billing_paused_by_user boolean DEFAULT false NOT NULL,
    billing_instance_service_is_active boolean DEFAULT false NOT NULL,
    billing_ended_at timestamp with time zone,
    settings jsonb DEFAULT '{}'::jsonb,
    features jsonb DEFAULT '{}'::jsonb,
    trial_ended_at timestamp with time zone,
    billing_discount real,
    billing_instance_service_updated_at timestamp with time zone,
    onboarding jsonb
);


--
-- Name: analytics_tables_row_count_view; Type: VIEW; Schema: public; Owner: -
--

CREATE VIEW public.analytics_tables_row_count_view AS
 SELECT 'tenants_analytics_view'::text AS view_name,
    count(*) AS row_count
   FROM public.tenants
UNION ALL
 SELECT 'license_limits_analytics_view'::text AS view_name,
    count(*) AS row_count
   FROM public.license_limits
UNION ALL
 SELECT 'licenses_analytics_view'::text AS view_name,
    count(*) AS row_count
   FROM public.licenses
UNION ALL
 SELECT 'license_quarantines_analytics_view'::text AS view_name,
    count(*) AS row_count
   FROM public.license_quarantines;


--
-- Name: collections; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.collections (
    collection_id bigint DEFAULT public.get_id() NOT NULL,
    title text NOT NULL,
    description text,
    parent_id bigint,
    tenant_id text DEFAULT 'common'::text NOT NULL,
    created_by text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_by text NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    deleted_by text,
    deleted_at timestamp with time zone,
    meta jsonb DEFAULT '{}'::jsonb NOT NULL,
    title_lower text NOT NULL,
    sort_title bytea NOT NULL
);


--
-- Name: color_palettes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.color_palettes (
    color_palette_id bigint DEFAULT public.get_id() NOT NULL,
    tenant_id text DEFAULT 'common'::text NOT NULL,
    name text NOT NULL,
    display_name text NOT NULL,
    colors jsonb DEFAULT '[]'::jsonb NOT NULL,
    is_gradient boolean DEFAULT false NOT NULL,
    is_default boolean DEFAULT false NOT NULL,
    CONSTRAINT color_palettes_non_empty_name_constraint CHECK ((TRIM(BOTH FROM name) <> ''::text))
);


--
-- Name: comments; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.comments (
    id uuid DEFAULT public.uuid_generate_v1mc() NOT NULL,
    feed text NOT NULL,
    creator_login text NOT NULL,
    created_date timestamp(0) with time zone DEFAULT now(),
    modifier_login text,
    modified_date timestamp(0) with time zone,
    date timestamp(0) with time zone NOT NULL,
    date_until timestamp(0) with time zone,
    type public.comment_type NOT NULL,
    text text NOT NULL,
    meta jsonb NOT NULL,
    params jsonb,
    is_removed boolean DEFAULT false,
    removed_date timestamp(0) with time zone,
    remover_login text,
    CONSTRAINT date_until_after_date CHECK (((date_until IS NULL) OR (date_until >= date))),
    CONSTRAINT is_removed_with_removed_date_with_remover_login CHECK ((((NOT is_removed) AND (removed_date IS NULL) AND (remover_login IS NULL)) OR (is_removed AND (removed_date IS NOT NULL) AND (remover_login IS NOT NULL)))),
    CONSTRAINT modified_date_after_created_date CHECK (((modified_date IS NULL) OR (modified_date > created_date))),
    CONSTRAINT modifier_login_with_modified_date CHECK ((((modifier_login IS NULL) AND (modified_date IS NULL)) OR ((modifier_login IS NOT NULL) AND (modified_date IS NOT NULL))))
);


--
-- Name: counter_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.counter_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: data_exports; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.data_exports (
    data_export_id bigint DEFAULT public.get_id() NOT NULL,
    tenant_id text DEFAULT 'common'::text NOT NULL,
    chart_id bigint,
    chart_rev_id bigint,
    dataset_id bigint,
    dataset_rev_id bigint,
    connection_id bigint,
    connection_rev_id bigint,
    params jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_by text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    expired_at timestamp with time zone,
    job_id text NOT NULL,
    s3_key text,
    error jsonb,
    upload_id text,
    status public.data_export_status DEFAULT 'IN_PROGRESS'::public.data_export_status NOT NULL,
    size bigint,
    finished_at timestamp with time zone,
    cancelled_by text,
    cancelled_at timestamp with time zone
);


--
-- Name: embedding_secrets; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.embedding_secrets (
    embedding_secret_id bigint DEFAULT public.get_id() NOT NULL,
    title text NOT NULL,
    workbook_id bigint NOT NULL,
    public_key text NOT NULL,
    created_by text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    type public.embed_type_enum,
    tenant_id text NOT NULL
);


--
-- Name: embeds; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.embeds (
    embed_id bigint DEFAULT public.get_id() NOT NULL,
    title text NOT NULL,
    embedding_secret_id bigint NOT NULL,
    entry_id bigint NOT NULL,
    deps_ids text[] DEFAULT '{}'::text[] NOT NULL,
    unsigned_params text[] DEFAULT '{}'::text[] NOT NULL,
    created_by text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    private_params text[] DEFAULT '{}'::text[] NOT NULL,
    public_params_mode boolean DEFAULT true NOT NULL,
    updated_by text NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    settings jsonb DEFAULT '{}'::jsonb,
    type public.embed_type_enum,
    allow_all_deps boolean DEFAULT false NOT NULL,
    tenant_id text NOT NULL
);


--
-- Name: entity_bindings; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.entity_bindings (
    source_id bigint NOT NULL,
    target_id bigint NOT NULL,
    target_type public.entity_bindings_target_type NOT NULL,
    is_delegated boolean DEFAULT false NOT NULL,
    created_by text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_by text NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: entries; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.entries (
    scope public.scope NOT NULL,
    type text NOT NULL,
    key text,
    inner_meta jsonb,
    created_by text NOT NULL,
    created_at timestamp with time zone DEFAULT now(),
    updated_by text NOT NULL,
    updated_at timestamp with time zone DEFAULT now(),
    is_deleted boolean DEFAULT false,
    deleted_at timestamp with time zone,
    hidden boolean DEFAULT false,
    display_key text,
    entry_id bigint DEFAULT public.get_id() NOT NULL,
    saved_id bigint,
    published_id bigint,
    tenant_id text,
    name text,
    sort_name bytea,
    public boolean DEFAULT false,
    unversioned_data jsonb DEFAULT '{}'::jsonb,
    workbook_id bigint,
    mirrored boolean DEFAULT false,
    collection_id bigint,
    CONSTRAINT entries_single_container_constraint CHECK ((NOT ((workbook_id IS NOT NULL) AND (collection_id IS NOT NULL))))
);


--
-- Name: favorites; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.favorites (
    entry_id bigint NOT NULL,
    tenant_id text NOT NULL,
    login text NOT NULL,
    created_at timestamp with time zone DEFAULT now(),
    alias text,
    display_alias text,
    sort_alias bytea,
    entity_id bigint NOT NULL,
    entity_type public.favorites_entity_type NOT NULL
);


--
-- Name: installation_settings; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.installation_settings (
    name text NOT NULL,
    data jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: license_limits_analytics_view; Type: VIEW; Schema: public; Owner: -
--

CREATE VIEW public.license_limits_analytics_view AS
 SELECT public.encode_id(license_limit_id) AS license_limit_id,
    tenant_id,
    type,
    started_at,
    creators_limit_value
   FROM public.license_limits;


--
-- Name: license_quarantines_analytics_view; Type: VIEW; Schema: public; Owner: -
--

CREATE VIEW public.license_quarantines_analytics_view AS
 SELECT public.encode_id(license_quarantine_id) AS license_quarantine_id,
    tenant_id,
    license_type,
    started_at,
    ends_at,
    meta
   FROM public.license_quarantines;


--
-- Name: licenses_analytics_view; Type: VIEW; Schema: public; Owner: -
--

CREATE VIEW public.licenses_analytics_view AS
 SELECT public.encode_id(license_id) AS license_id,
    tenant_id,
    user_id,
    expires_at,
    created_by,
    created_at
   FROM public.licenses;


--
-- Name: links; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.links (
    from_id bigint NOT NULL,
    to_id bigint NOT NULL,
    name text NOT NULL
);


--
-- Name: locks; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.locks (
    lock_id bigint DEFAULT public.get_id() NOT NULL,
    entry_id bigint NOT NULL,
    lock_token text NOT NULL,
    expiry_date timestamp with time zone NOT NULL,
    login text,
    start_date timestamp with time zone DEFAULT now()
);


--
-- Name: migrations; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.migrations (
    id integer NOT NULL,
    name character varying(255),
    batch integer,
    migration_time timestamp with time zone
);


--
-- Name: migrations_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.migrations_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: migrations_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.migrations_id_seq OWNED BY public.migrations.id;


--
-- Name: migrations_lock; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.migrations_lock (
    index integer NOT NULL,
    is_locked integer
);


--
-- Name: migrations_lock_index_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.migrations_lock_index_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: migrations_lock_index_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.migrations_lock_index_seq OWNED BY public.migrations_lock.index;


--
-- Name: operations; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.operations (
    operation_id bigint DEFAULT public.get_id() NOT NULL,
    type text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    created_by text NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    result jsonb DEFAULT '{}'::jsonb NOT NULL,
    status public.operation_status_enum DEFAULT 'scheduled'::public.operation_status_enum NOT NULL,
    meta jsonb DEFAULT '{}'::jsonb NOT NULL,
    inner_meta jsonb DEFAULT '{}'::jsonb NOT NULL,
    run_after timestamp with time zone DEFAULT now() NOT NULL,
    retries_left smallint DEFAULT 3 NOT NULL,
    retries_interval_sec integer DEFAULT 180 NOT NULL,
    tenant_id text
);


--
-- Name: presets; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.presets (
    preset_id bigint DEFAULT public.get_id() NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    data jsonb DEFAULT '{}'::jsonb NOT NULL
);


--
-- Name: revisions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.revisions (
    data jsonb,
    meta jsonb,
    created_by text NOT NULL,
    created_at timestamp with time zone DEFAULT now(),
    updated_by text NOT NULL,
    updated_at timestamp with time zone DEFAULT now(),
    rev_id bigint DEFAULT public.get_id() NOT NULL,
    entry_id bigint,
    links jsonb,
    annotation jsonb,
    version integer,
    source_version integer
);


--
-- Name: states; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.states (
    hash text NOT NULL,
    entry_id bigint NOT NULL,
    data jsonb,
    created_at timestamp with time zone DEFAULT now()
);


--
-- Name: subscription_recipients; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.subscription_recipients (
    subscription_id bigint NOT NULL,
    user_id text NOT NULL,
    transport public.subscription_recipient_transport_type_enum NOT NULL,
    created_by text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: subscriptions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.subscriptions (
    subscription_id bigint DEFAULT public.get_id() NOT NULL,
    title text NOT NULL,
    description text,
    meta jsonb DEFAULT '{}'::jsonb NOT NULL,
    status public.subscription_status_enum DEFAULT 'active'::public.subscription_status_enum NOT NULL,
    tenant_id text DEFAULT 'common'::text NOT NULL,
    workbook_id bigint NOT NULL,
    created_by text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_by text NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    suspended_at timestamp with time zone,
    content_type public.subscription_content_type_enum NOT NULL,
    content_entry_id bigint NOT NULL,
    content_options jsonb DEFAULT '{}'::jsonb NOT NULL,
    trigger_type public.subscription_trigger_type_enum NOT NULL,
    trigger_entry_id bigint,
    trigger_options jsonb DEFAULT '{}'::jsonb NOT NULL,
    artifact_type public.subscription_artifact_type_enum NOT NULL,
    artifact_options jsonb DEFAULT '{}'::jsonb NOT NULL,
    embed_id bigint,
    embed_private_key jsonb DEFAULT '{}'::jsonb
);


--
-- Name: templates; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.templates (
    name text NOT NULL,
    data jsonb
);


--
-- Name: tenants_analytics_view; Type: VIEW; Schema: public; Owner: -
--

CREATE VIEW public.tenants_analytics_view AS
 SELECT tenant_id,
    created_at,
    billing_instance_service_id,
    billing_instance_service_is_active,
    billing_instance_service_updated_at,
    trial_ended_at,
    billing_discount,
    settings,
    meta
   FROM public.tenants;


--
-- Name: user_settings; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.user_settings (
    user_id text NOT NULL,
    settings jsonb DEFAULT '{}'::jsonb NOT NULL,
    updated_at timestamp with time zone DEFAULT now(),
    tenant_id text DEFAULT 'common'::text NOT NULL
);


--
-- Name: workbooks; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.workbooks (
    workbook_id bigint DEFAULT public.get_id() NOT NULL,
    title text NOT NULL,
    description text,
    tenant_id text DEFAULT 'common'::text NOT NULL,
    meta jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_by text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    deleted_at timestamp with time zone,
    is_template boolean DEFAULT false,
    collection_id bigint,
    deleted_by text,
    updated_by text,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    title_lower text NOT NULL,
    sort_title bytea NOT NULL,
    status public.workbook_status_enum DEFAULT 'active'::public.workbook_status_enum NOT NULL
);


--
-- Name: migrations id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.migrations ALTER COLUMN id SET DEFAULT nextval('public.migrations_id_seq'::regclass);


--
-- Name: migrations_lock index; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.migrations_lock ALTER COLUMN index SET DEFAULT nextval('public.migrations_lock_index_seq'::regclass);


--
-- Name: collections collections_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.collections
    ADD CONSTRAINT collections_pkey PRIMARY KEY (collection_id);


--
-- Name: color_palettes color_palettes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.color_palettes
    ADD CONSTRAINT color_palettes_pkey PRIMARY KEY (color_palette_id);


--
-- Name: color_palettes color_palettes_uniq_name_constraint; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.color_palettes
    ADD CONSTRAINT color_palettes_uniq_name_constraint UNIQUE (tenant_id, name);


--
-- Name: comments comments_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.comments
    ADD CONSTRAINT comments_pkey PRIMARY KEY (id);


--
-- Name: data_exports data_exports_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.data_exports
    ADD CONSTRAINT data_exports_pkey PRIMARY KEY (data_export_id);


--
-- Name: embedding_secrets embedding_secrets_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.embedding_secrets
    ADD CONSTRAINT embedding_secrets_pkey PRIMARY KEY (embedding_secret_id);


--
-- Name: embeds embeds_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.embeds
    ADD CONSTRAINT embeds_pkey PRIMARY KEY (embed_id);


--
-- Name: entity_bindings entity_bindings_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.entity_bindings
    ADD CONSTRAINT entity_bindings_pkey PRIMARY KEY (source_id, target_id);


--
-- Name: entries entries_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.entries
    ADD CONSTRAINT entries_pkey PRIMARY KEY (entry_id);


--
-- Name: entries entries_uniq_entry_id_tenant_id_constraint; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.entries
    ADD CONSTRAINT entries_uniq_entry_id_tenant_id_constraint UNIQUE (entry_id, tenant_id);


--
-- Name: favorites favorites_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.favorites
    ADD CONSTRAINT favorites_pkey PRIMARY KEY (entry_id, login);


--
-- Name: installation_settings installation_settings_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.installation_settings
    ADD CONSTRAINT installation_settings_pkey PRIMARY KEY (name);


--
-- Name: license_limits license_limits_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.license_limits
    ADD CONSTRAINT license_limits_pkey PRIMARY KEY (license_limit_id);


--
-- Name: license_quarantines license_quarantines_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.license_quarantines
    ADD CONSTRAINT license_quarantines_pkey PRIMARY KEY (license_quarantine_id);


--
-- Name: licenses licenses_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.licenses
    ADD CONSTRAINT licenses_pkey PRIMARY KEY (license_id);


--
-- Name: links links_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.links
    ADD CONSTRAINT links_pkey PRIMARY KEY (from_id, to_id, name);


--
-- Name: locks locks_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.locks
    ADD CONSTRAINT locks_pkey PRIMARY KEY (lock_id);


--
-- Name: migrations_lock migrations_lock_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.migrations_lock
    ADD CONSTRAINT migrations_lock_pkey PRIMARY KEY (index);


--
-- Name: migrations migrations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.migrations
    ADD CONSTRAINT migrations_pkey PRIMARY KEY (id);


--
-- Name: operations operations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.operations
    ADD CONSTRAINT operations_pkey PRIMARY KEY (operation_id);


--
-- Name: presets presets_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.presets
    ADD CONSTRAINT presets_pkey PRIMARY KEY (preset_id);


--
-- Name: revisions revisions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.revisions
    ADD CONSTRAINT revisions_pkey PRIMARY KEY (rev_id);


--
-- Name: states states_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.states
    ADD CONSTRAINT states_pkey PRIMARY KEY (hash, entry_id);


--
-- Name: subscription_recipients subscription_recipients_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.subscription_recipients
    ADD CONSTRAINT subscription_recipients_pkey PRIMARY KEY (subscription_id, user_id, transport);


--
-- Name: subscriptions subscriptions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.subscriptions
    ADD CONSTRAINT subscriptions_pkey PRIMARY KEY (subscription_id);


--
-- Name: templates templates_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.templates
    ADD CONSTRAINT templates_pkey PRIMARY KEY (name);


--
-- Name: tenants tenants_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tenants
    ADD CONSTRAINT tenants_pkey PRIMARY KEY (tenant_id);


--
-- Name: locks uniq_active_lock_per_entry_id; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.locks
    ADD CONSTRAINT uniq_active_lock_per_entry_id EXCLUDE USING gist (entry_id WITH =, tstzrange(start_date, expiry_date) WITH &&) WHERE ((expiry_date > start_date));


--
-- Name: entries uniq_scope_name_workbook_id; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.entries
    ADD CONSTRAINT uniq_scope_name_workbook_id UNIQUE (scope, name, workbook_id);


--
-- Name: user_settings user_settings_сomposite_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_settings
    ADD CONSTRAINT "user_settings_сomposite_pkey" PRIMARY KEY (user_id, tenant_id);


--
-- Name: workbooks workbooks_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.workbooks
    ADD CONSTRAINT workbooks_pkey PRIMARY KEY (workbook_id);


--
-- Name: collections_created_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX collections_created_at_idx ON public.collections USING btree (created_at);


--
-- Name: collections_parent_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX collections_parent_id_idx ON public.collections USING btree (parent_id);


--
-- Name: collections_sort_title_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX collections_sort_title_idx ON public.collections USING btree (sort_title);


--
-- Name: collections_tenant_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX collections_tenant_id_idx ON public.collections USING btree (tenant_id);


--
-- Name: collections_uniq_title_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX collections_uniq_title_idx ON public.collections USING btree (COALESCE((parent_id)::text, tenant_id), title_lower) WHERE (deleted_at IS NULL);


--
-- Name: collections_updated_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX collections_updated_at_idx ON public.collections USING btree (updated_at);


--
-- Name: color_palettes_tenant_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX color_palettes_tenant_id_idx ON public.color_palettes USING btree (tenant_id);


--
-- Name: color_palettes_uniq_default_for_tenant_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX color_palettes_uniq_default_for_tenant_id_idx ON public.color_palettes USING btree (tenant_id, is_default, is_gradient) WHERE (is_default = true);


--
-- Name: comments_date_until_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX comments_date_until_idx ON public.comments USING btree (date_until);


--
-- Name: comments_is_removed_feed_date_date_until_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX comments_is_removed_feed_date_date_until_idx ON public.comments USING btree (is_removed, feed, date, date_until);


--
-- Name: comments_is_removed_feed_lower_date_date_until_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX comments_is_removed_feed_lower_date_date_until_idx ON public.comments USING btree (is_removed, lower(feed), date, date_until);


--
-- Name: data_exports_chart_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX data_exports_chart_id_idx ON public.data_exports USING btree (chart_id);


--
-- Name: data_exports_connection_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX data_exports_connection_id_idx ON public.data_exports USING btree (connection_id);


--
-- Name: data_exports_dataset_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX data_exports_dataset_id_idx ON public.data_exports USING btree (dataset_id);


--
-- Name: data_exports_expired_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX data_exports_expired_at_idx ON public.data_exports USING btree (expired_at);


--
-- Name: embedding_secrets_workbook_id_type_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX embedding_secrets_workbook_id_type_idx ON public.embedding_secrets USING btree (workbook_id, type);


--
-- Name: embeds_embedding_secret_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX embeds_embedding_secret_id_idx ON public.embeds USING btree (embedding_secret_id);


--
-- Name: embeds_entry_id_type_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX embeds_entry_id_type_idx ON public.embeds USING btree (entry_id, type);


--
-- Name: embeds_updated_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX embeds_updated_at_idx ON public.embeds USING btree (updated_at);


--
-- Name: entity_bindings_source_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX entity_bindings_source_id_idx ON public.entity_bindings USING btree (source_id);


--
-- Name: entity_bindings_target_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX entity_bindings_target_id_idx ON public.entity_bindings USING btree (target_id);


--
-- Name: entity_bindings_target_type_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX entity_bindings_target_type_idx ON public.entity_bindings USING btree (target_type);


--
-- Name: entries_collection_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX entries_collection_id_idx ON public.entries USING btree (collection_id);


--
-- Name: entries_created_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX entries_created_at_idx ON public.entries USING btree (created_at);


--
-- Name: entries_created_by_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX entries_created_by_idx ON public.entries USING btree (created_by);


--
-- Name: entries_deleted_sort_name_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX entries_deleted_sort_name_idx ON public.entries USING btree (tenant_id, sort_name, entry_id) WHERE ((is_deleted = true) AND ((workbook_id IS NOT NULL) OR (collection_id IS NOT NULL)));


--
-- Name: entries_deleted_updated_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX entries_deleted_updated_at_idx ON public.entries USING btree (tenant_id, updated_at, entry_id) WHERE ((is_deleted = true) AND ((workbook_id IS NOT NULL) OR (collection_id IS NOT NULL)));


--
-- Name: entries_entry_id_revisions_cover_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX entries_entry_id_revisions_cover_idx ON public.entries USING btree (entry_id) INCLUDE (saved_id, published_id, scope);


--
-- Name: entries_tenant_id_parent_folder_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX entries_tenant_id_parent_folder_idx ON public.entries USING btree (tenant_id, rtrim(rtrim(key, '/'::text), name));


--
-- Name: entries_tenant_id_scope_created_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX entries_tenant_id_scope_created_at_idx ON public.entries USING btree (tenant_id, scope, created_at);


--
-- Name: entries_tenant_id_scope_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX entries_tenant_id_scope_idx ON public.entries USING btree (tenant_id, scope);


--
-- Name: entries_tenant_id_scope_sort_name_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX entries_tenant_id_scope_sort_name_idx ON public.entries USING btree (tenant_id, scope, sort_name);


--
-- Name: entries_uniq_scope_name_conllection_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX entries_uniq_scope_name_conllection_id_idx ON public.entries USING btree (scope, name, collection_id);


--
-- Name: entries_updated_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX entries_updated_at_idx ON public.entries USING btree (updated_at);


--
-- Name: entries_workbook_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX entries_workbook_id_idx ON public.entries USING btree (workbook_id);


--
-- Name: favorites_alias_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX favorites_alias_idx ON public.favorites USING btree (alias);


--
-- Name: favorites_entity_id_login_uniq_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX favorites_entity_id_login_uniq_idx ON public.favorites USING btree (entity_id, login);


--
-- Name: favorites_sort_alias_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX favorites_sort_alias_idx ON public.favorites USING btree (sort_alias);


--
-- Name: from_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX from_id_idx ON public.links USING btree (from_id);


--
-- Name: idx_data_exports_created_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_data_exports_created_at ON public.data_exports USING btree (created_at);


--
-- Name: idx_data_exports_expired_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_data_exports_expired_at ON public.data_exports USING btree (expired_at);


--
-- Name: idx_data_exports_finished_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_data_exports_finished_at ON public.data_exports USING btree (finished_at);


--
-- Name: idx_data_exports_size; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_data_exports_size ON public.data_exports USING btree (size);


--
-- Name: idx_data_exports_status; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX idx_data_exports_status ON public.data_exports USING btree (status);


--
-- Name: key_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX key_idx ON public.entries USING gin (key public.gin_trgm_ops);


--
-- Name: license_limits_creators_limit_value_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX license_limits_creators_limit_value_idx ON public.license_limits USING btree (creators_limit_value);


--
-- Name: license_limits_started_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX license_limits_started_at_idx ON public.license_limits USING btree (started_at);


--
-- Name: license_limits_tenant_id_started_at_desc_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX license_limits_tenant_id_started_at_desc_idx ON public.license_limits USING btree (tenant_id, started_at DESC);


--
-- Name: license_quarantines_tenant_id_license_type_ends_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX license_quarantines_tenant_id_license_type_ends_at_idx ON public.license_quarantines USING btree (tenant_id, license_type, ends_at);


--
-- Name: licenses_quarantine_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX licenses_quarantine_id_idx ON public.licenses USING btree (quarantine_id);


--
-- Name: licenses_tenant_id_created_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX licenses_tenant_id_created_at_idx ON public.licenses USING btree (tenant_id, created_at);


--
-- Name: licenses_tenant_id_expires_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX licenses_tenant_id_expires_at_idx ON public.licenses USING btree (tenant_id, expires_at);


--
-- Name: licenses_tenant_id_license_type_expires_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX licenses_tenant_id_license_type_expires_at_idx ON public.licenses USING btree (tenant_id, license_type, expires_at);


--
-- Name: licenses_tenant_id_updated_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX licenses_tenant_id_updated_at_idx ON public.licenses USING btree (tenant_id, updated_at);


--
-- Name: licenses_tenant_id_user_id_license_type_unique_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX licenses_tenant_id_user_id_license_type_unique_idx ON public.licenses USING btree (tenant_id, user_id, license_type);


--
-- Name: locks_entry_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX locks_entry_id_idx ON public.locks USING btree (entry_id);


--
-- Name: operations_retries_left_run_after_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX operations_retries_left_run_after_idx ON public.operations USING btree (retries_left, run_after) WHERE (status = 'scheduled'::public.operation_status_enum);


--
-- Name: public_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX public_idx ON public.entries USING btree (public);


--
-- Name: revisions_entry_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX revisions_entry_id_idx ON public.revisions USING btree (entry_id);


--
-- Name: revisions_meta_mdb_cluster_id_index; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX revisions_meta_mdb_cluster_id_index ON public.revisions USING gin (((meta ->> 'mdb_cluster_id'::text)));


--
-- Name: revisions_updated_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX revisions_updated_at_idx ON public.revisions USING btree (updated_at);


--
-- Name: states_entry_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX states_entry_id_idx ON public.states USING btree (entry_id);


--
-- Name: subscription_recipients_user_id_transport_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX subscription_recipients_user_id_transport_idx ON public.subscription_recipients USING btree (user_id, transport);


--
-- Name: subscriptions_embed_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX subscriptions_embed_id_idx ON public.subscriptions USING btree (embed_id) WHERE (embed_id IS NOT NULL);


--
-- Name: subscriptions_tenant_id_content_entry_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX subscriptions_tenant_id_content_entry_id_idx ON public.subscriptions USING btree (tenant_id, content_entry_id);


--
-- Name: subscriptions_tenant_id_created_by_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX subscriptions_tenant_id_created_by_idx ON public.subscriptions USING btree (tenant_id, created_by);


--
-- Name: subscriptions_tenant_id_workbook_id_created_at_desc_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX subscriptions_tenant_id_workbook_id_created_at_desc_idx ON public.subscriptions USING btree (tenant_id, workbook_id, created_at DESC);


--
-- Name: subscriptions_title_description_trgm_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX subscriptions_title_description_trgm_idx ON public.subscriptions USING gin ((((title || ' '::text) || description)) public.gin_trgm_ops);


--
-- Name: tenant_id_key_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX tenant_id_key_idx ON public.entries USING btree (tenant_id, key);


--
-- Name: tenant_id_plus_login_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX tenant_id_plus_login_idx ON public.favorites USING btree (tenant_id, login);


--
-- Name: tenants_billing_instance_service_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX tenants_billing_instance_service_id_idx ON public.tenants USING btree (billing_instance_service_id);


--
-- Name: tenants_enabled_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX tenants_enabled_idx ON public.tenants USING btree (enabled);


--
-- Name: tenants_tenant_id_uindex; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX tenants_tenant_id_uindex ON public.tenants USING btree (tenant_id);


--
-- Name: to_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX to_id_idx ON public.links USING btree (to_id);


--
-- Name: workbooks_collection_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX workbooks_collection_id_idx ON public.workbooks USING btree (collection_id);


--
-- Name: workbooks_created_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX workbooks_created_at_idx ON public.workbooks USING btree (created_at);


--
-- Name: workbooks_sort_title_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX workbooks_sort_title_idx ON public.workbooks USING btree (sort_title);


--
-- Name: workbooks_tenant_id_deleted_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX workbooks_tenant_id_deleted_at_idx ON public.workbooks USING btree (tenant_id, deleted_at) WHERE (deleted_at IS NOT NULL);


--
-- Name: workbooks_tenant_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX workbooks_tenant_id_idx ON public.workbooks USING btree (tenant_id);


--
-- Name: workbooks_uniq_title_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX workbooks_uniq_title_idx ON public.workbooks USING btree (COALESCE((collection_id)::text, tenant_id), title_lower) WHERE (deleted_at IS NULL);


--
-- Name: workbooks_updated_at_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX workbooks_updated_at_idx ON public.workbooks USING btree (updated_at);


--
-- Name: collections before_collections_insert_or_update; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER before_collections_insert_or_update BEFORE INSERT OR UPDATE ON public.collections FOR EACH ROW EXECUTE FUNCTION public.update_collections();


--
-- Name: entries before_entries_insert_or_update; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER before_entries_insert_or_update BEFORE INSERT OR UPDATE ON public.entries FOR EACH ROW EXECUTE FUNCTION public.update_entries();


--
-- Name: favorites before_favorites_insert_or_update; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER before_favorites_insert_or_update BEFORE INSERT OR UPDATE ON public.favorites FOR EACH ROW EXECUTE FUNCTION public.update_favorites();


--
-- Name: workbooks before_workbooks_insert_or_update; Type: TRIGGER; Schema: public; Owner: -
--

CREATE TRIGGER before_workbooks_insert_or_update BEFORE INSERT OR UPDATE ON public.workbooks FOR EACH ROW EXECUTE FUNCTION public.update_workbooks();


--
-- Name: collections collections_parent_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.collections
    ADD CONSTRAINT collections_parent_id_fkey FOREIGN KEY (parent_id) REFERENCES public.collections(collection_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: collections collections_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.collections
    ADD CONSTRAINT collections_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(tenant_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: color_palettes color_palettes_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.color_palettes
    ADD CONSTRAINT color_palettes_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(tenant_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: data_exports data_exports_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.data_exports
    ADD CONSTRAINT data_exports_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(tenant_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: embedding_secrets embedding_secrets_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.embedding_secrets
    ADD CONSTRAINT embedding_secrets_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(tenant_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: embedding_secrets embedding_secrets_workbook_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.embedding_secrets
    ADD CONSTRAINT embedding_secrets_workbook_id_fkey FOREIGN KEY (workbook_id) REFERENCES public.workbooks(workbook_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: embeds embeds_embedding_secret_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.embeds
    ADD CONSTRAINT embeds_embedding_secret_id_fkey FOREIGN KEY (embedding_secret_id) REFERENCES public.embedding_secrets(embedding_secret_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: embeds embeds_entry_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.embeds
    ADD CONSTRAINT embeds_entry_id_fkey FOREIGN KEY (entry_id) REFERENCES public.entries(entry_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: embeds embeds_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.embeds
    ADD CONSTRAINT embeds_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(tenant_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: entity_bindings entity_bindings_source_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.entity_bindings
    ADD CONSTRAINT entity_bindings_source_id_fkey FOREIGN KEY (source_id) REFERENCES public.entries(entry_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: entries entries_collection_id_ref; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.entries
    ADD CONSTRAINT entries_collection_id_ref FOREIGN KEY (collection_id) REFERENCES public.collections(collection_id) ON DELETE CASCADE;


--
-- Name: entries entries_tenants_id; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.entries
    ADD CONSTRAINT entries_tenants_id FOREIGN KEY (tenant_id) REFERENCES public.tenants(tenant_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: entries entries_workbook_id_ref; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.entries
    ADD CONSTRAINT entries_workbook_id_ref FOREIGN KEY (workbook_id) REFERENCES public.workbooks(workbook_id) ON DELETE CASCADE;


--
-- Name: favorites favorites_entry_id_tenant_id_constraint; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.favorites
    ADD CONSTRAINT favorites_entry_id_tenant_id_constraint FOREIGN KEY (entry_id, tenant_id) REFERENCES public.entries(entry_id, tenant_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: license_limits license_limits_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.license_limits
    ADD CONSTRAINT license_limits_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(tenant_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: license_quarantines license_quarantines_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.license_quarantines
    ADD CONSTRAINT license_quarantines_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(tenant_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: licenses licenses_quarantine_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.licenses
    ADD CONSTRAINT licenses_quarantine_id_fkey FOREIGN KEY (quarantine_id) REFERENCES public.license_quarantines(license_quarantine_id) ON DELETE SET NULL;


--
-- Name: licenses licenses_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.licenses
    ADD CONSTRAINT licenses_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(tenant_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: locks locks_entries_id; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.locks
    ADD CONSTRAINT locks_entries_id FOREIGN KEY (entry_id) REFERENCES public.entries(entry_id) ON DELETE CASCADE;


--
-- Name: operations operations_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.operations
    ADD CONSTRAINT operations_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(tenant_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: revisions revisions_entries_id; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.revisions
    ADD CONSTRAINT revisions_entries_id FOREIGN KEY (entry_id) REFERENCES public.entries(entry_id) ON DELETE CASCADE;


--
-- Name: states states_entries_id; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.states
    ADD CONSTRAINT states_entries_id FOREIGN KEY (entry_id) REFERENCES public.entries(entry_id) ON DELETE CASCADE;


--
-- Name: subscription_recipients subscription_recipients_subscription_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.subscription_recipients
    ADD CONSTRAINT subscription_recipients_subscription_id_fkey FOREIGN KEY (subscription_id) REFERENCES public.subscriptions(subscription_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: subscriptions subscriptions_content_entry_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.subscriptions
    ADD CONSTRAINT subscriptions_content_entry_id_fkey FOREIGN KEY (content_entry_id) REFERENCES public.entries(entry_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: subscriptions subscriptions_embed_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.subscriptions
    ADD CONSTRAINT subscriptions_embed_id_fkey FOREIGN KEY (embed_id) REFERENCES public.embeds(embed_id) ON DELETE SET NULL;


--
-- Name: subscriptions subscriptions_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.subscriptions
    ADD CONSTRAINT subscriptions_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(tenant_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: subscriptions subscriptions_workbook_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.subscriptions
    ADD CONSTRAINT subscriptions_workbook_id_fkey FOREIGN KEY (workbook_id) REFERENCES public.workbooks(workbook_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: user_settings user_settings_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.user_settings
    ADD CONSTRAINT user_settings_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(tenant_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: workbooks workbooks_collection_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.workbooks
    ADD CONSTRAINT workbooks_collection_id_fkey FOREIGN KEY (collection_id) REFERENCES public.collections(collection_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: workbooks workbooks_tenant_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.workbooks
    ADD CONSTRAINT workbooks_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(tenant_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- PostgreSQL database dump complete
--


