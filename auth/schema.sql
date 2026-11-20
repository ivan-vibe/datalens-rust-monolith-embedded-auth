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
-- Name: auth_get_id(); Type: FUNCTION; Schema: public; Owner: -
--

CREATE FUNCTION public.auth_get_id(OUT result bigint) RETURNS bigint
    LANGUAGE plpgsql
    AS $$
        DECLARE
            our_epoch bigint := 1514754000000;
            seq_id bigint;
            now_millis bigint;
            shard_id int := 1;
        BEGIN
            SELECT nextval('auth_counter_seq') % 4096 INTO seq_id;

            SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000) INTO now_millis;
            result := (now_millis - our_epoch) << 23;
            result := result | (shard_id << 10);
            result := result | (seq_id);
        END;
        $$;


--
-- Name: auth_counter_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.auth_counter_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: auth_migrations; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.auth_migrations (
    id integer NOT NULL,
    name character varying(255),
    batch integer,
    migration_time timestamp with time zone
);


--
-- Name: auth_migrations_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.auth_migrations_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: auth_migrations_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.auth_migrations_id_seq OWNED BY public.auth_migrations.id;


--
-- Name: auth_migrations_lock; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.auth_migrations_lock (
    index integer NOT NULL,
    is_locked integer
);


--
-- Name: auth_migrations_lock_index_seq; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.auth_migrations_lock_index_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: auth_migrations_lock_index_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.auth_migrations_lock_index_seq OWNED BY public.auth_migrations_lock.index;


--
-- Name: auth_refresh_tokens; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.auth_refresh_tokens (
    refresh_token_id bigint DEFAULT public.auth_get_id() NOT NULL,
    session_id bigint NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    expired_at timestamp with time zone NOT NULL
);


--
-- Name: auth_roles; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.auth_roles (
    role_id bigint DEFAULT public.auth_get_id() NOT NULL,
    user_id bigint NOT NULL,
    role text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: auth_sessions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.auth_sessions (
    session_id bigint DEFAULT public.auth_get_id() NOT NULL,
    user_id bigint NOT NULL,
    user_agent text NOT NULL,
    user_ip text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    expired_at timestamp with time zone NOT NULL
);


--
-- Name: auth_users; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.auth_users (
    user_id bigint DEFAULT public.auth_get_id() NOT NULL,
    login text,
    password text,
    first_name text,
    last_name text,
    email text,
    idp_user_id text,
    idp_slug text,
    idp_type text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


--
-- Name: auth_migrations id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.auth_migrations ALTER COLUMN id SET DEFAULT nextval('public.auth_migrations_id_seq'::regclass);


--
-- Name: auth_migrations_lock index; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.auth_migrations_lock ALTER COLUMN index SET DEFAULT nextval('public.auth_migrations_lock_index_seq'::regclass);


--
-- Name: auth_migrations_lock auth_migrations_lock_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.auth_migrations_lock
    ADD CONSTRAINT auth_migrations_lock_pkey PRIMARY KEY (index);


--
-- Name: auth_migrations auth_migrations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.auth_migrations
    ADD CONSTRAINT auth_migrations_pkey PRIMARY KEY (id);


--
-- Name: auth_refresh_tokens auth_refresh_tokens_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.auth_refresh_tokens
    ADD CONSTRAINT auth_refresh_tokens_pkey PRIMARY KEY (refresh_token_id);


--
-- Name: auth_roles auth_roles_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.auth_roles
    ADD CONSTRAINT auth_roles_pkey PRIMARY KEY (role_id);


--
-- Name: auth_sessions auth_sessions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.auth_sessions
    ADD CONSTRAINT auth_sessions_pkey PRIMARY KEY (session_id);


--
-- Name: auth_users auth_users_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.auth_users
    ADD CONSTRAINT auth_users_pkey PRIMARY KEY (user_id);


--
-- Name: auth_refresh_tokens_session_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX auth_refresh_tokens_session_id_idx ON public.auth_refresh_tokens USING btree (session_id);


--
-- Name: auth_roles_role_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX auth_roles_role_idx ON public.auth_roles USING btree (role);


--
-- Name: auth_roles_uniq_user_role_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX auth_roles_uniq_user_role_idx ON public.auth_roles USING btree (user_id, role);


--
-- Name: auth_roles_user_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX auth_roles_user_id_idx ON public.auth_roles USING btree (user_id);


--
-- Name: auth_sessions_user_id_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX auth_sessions_user_id_idx ON public.auth_sessions USING btree (user_id);


--
-- Name: auth_users_email_lower_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX auth_users_email_lower_idx ON public.auth_users USING btree (lower(email));


--
-- Name: auth_users_email_trgm_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX auth_users_email_trgm_idx ON public.auth_users USING gin (lower(email) public.gin_trgm_ops);


--
-- Name: auth_users_first_name_lower_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX auth_users_first_name_lower_idx ON public.auth_users USING btree (lower(first_name));


--
-- Name: auth_users_first_name_trgm_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX auth_users_first_name_trgm_idx ON public.auth_users USING gin (lower(first_name) public.gin_trgm_ops);


--
-- Name: auth_users_idp_type_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX auth_users_idp_type_idx ON public.auth_users USING btree (idp_type);


--
-- Name: auth_users_last_name_lower_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX auth_users_last_name_lower_idx ON public.auth_users USING btree (lower(last_name));


--
-- Name: auth_users_last_name_trgm_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX auth_users_last_name_trgm_idx ON public.auth_users USING gin (lower(last_name) public.gin_trgm_ops);


--
-- Name: auth_users_login_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX auth_users_login_idx ON public.auth_users USING btree (login);


--
-- Name: auth_users_login_lower_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX auth_users_login_lower_idx ON public.auth_users USING btree (lower(login));


--
-- Name: auth_users_login_trgm_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX auth_users_login_trgm_idx ON public.auth_users USING gin (lower(login) public.gin_trgm_ops);


--
-- Name: auth_users_uniq_idp_user_slug_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX auth_users_uniq_idp_user_slug_idx ON public.auth_users USING btree (idp_user_id, idp_slug);


--
-- Name: auth_users_uniq_local_login_idx; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX auth_users_uniq_local_login_idx ON public.auth_users USING btree (lower(login)) WHERE (idp_slug IS NULL);


--
-- Name: auth_refresh_tokens auth_refresh_tokens_session_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.auth_refresh_tokens
    ADD CONSTRAINT auth_refresh_tokens_session_id_fkey FOREIGN KEY (session_id) REFERENCES public.auth_sessions(session_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: auth_roles auth_roles_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.auth_roles
    ADD CONSTRAINT auth_roles_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.auth_users(user_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- Name: auth_sessions auth_sessions_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.auth_sessions
    ADD CONSTRAINT auth_sessions_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.auth_users(user_id) ON UPDATE CASCADE ON DELETE CASCADE;


--
-- PostgreSQL database dump complete
--


