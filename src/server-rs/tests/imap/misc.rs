//! The Bun server's `capabilities.test.ts`, `session-buffer.test.ts` and
//! `trace.test.ts`, case for case.

mod capabilities {
    use mail_server::imap::capabilities::{capabilities, get_capabilities};

    // The Bun tests point `SSL_CERTIFICATE` at files that exist or do not.
    // Whether a certificate is readable is `tls::is_tls_available`'s to
    // decide, and tests in one process share one environment, so these pass
    // that answer in directly.

    fn list(is_tls: bool, sso: bool, tls_available: bool) -> Vec<String> {
        capabilities(is_tls, sso, tls_available).split(' ').map(str::to_string).collect()
    }

    fn has(list: &[String], capability: &str) -> bool {
        list.iter().any(|item| item == capability)
    }

    #[test]
    fn advertises_starttls_on_the_plain_port_when_the_certificate_exists() {
        assert!(has(&list(false, false, true), "STARTTLS"));
    }

    #[test]
    fn does_not_advertise_starttls_on_the_tls_wrapped_port() {
        assert!(!has(&list(true, false, true), "STARTTLS"));
        assert!(!get_capabilities(true, false).contains("STARTTLS"));
    }

    #[test]
    fn does_not_advertise_starttls_when_no_certificate_is_readable() {
        // Not configured, files absent, or only the key absent: all "not available".
        assert!(!has(&list(false, false, false), "STARTTLS"));
    }

    #[test]
    fn keeps_every_other_capability_when_starttls_is_withheld() {
        assert_eq!(
            list(false, false, false),
            ["IMAP4rev1", "LITERAL+", "SASL-IR", "LOGIN-REFERRALS", "ID", "ENABLE", "IDLE", "MOVE", "CONDSTORE", "AUTH=PLAIN"]
        );
    }

    #[test]
    fn does_not_advertise_special_use() {
        // RFC 6154 §2: the LIST attributes need no capability, and the parser
        // rejects `LIST (SPECIAL-USE) "" "*"`.
        assert!(!has(&list(false, false, true), "SPECIAL-USE"));
        assert!(!has(&list(true, false, true), "SPECIAL-USE"));
    }

    #[test]
    fn with_single_sign_on_disables_sign_in_before_tls_in_place_of_auth_plain() {
        let plain = list(false, true, false);
        assert!(has(&plain, "LOGINDISABLED"));
        assert!(!has(&plain, "AUTH=PLAIN"));
        assert_eq!(plain.last().map(String::as_str), Some("LOGINDISABLED"));
    }

    #[test]
    fn with_single_sign_on_offers_sign_in_once_on_tls() {
        let tls = list(true, true, false);
        assert!(has(&tls, "AUTH=PLAIN"));
        assert!(!has(&tls, "LOGINDISABLED"));
    }
}

mod session_buffer {
    use mail_server::imap::session_buffer::SessionBuffer;

    const SEGMENT_CEILING: usize = 4096;

    /// Octets the reader copied while `run` executed, and how many times.
    fn account_copies(buffer: &mut SessionBuffer, run: impl FnOnce(&mut SessionBuffer)) -> (u64, u64) {
        let before = buffer.cost();
        run(buffer);
        let after = buffer.cost();
        (after.copies - before.copies, after.copied_bytes - before.copied_bytes)
    }

    fn read_line(buffer: &mut SessionBuffer) -> Option<String> {
        let end = buffer.index_of_crlf()?;
        let line = buffer.decode(0, end);
        buffer.consume(end + 2);
        Some(line)
    }

    fn push(buffer: &mut SessionBuffer, text: &str) {
        buffer.push(text.as_bytes().to_vec());
    }

    #[test]
    fn takes_segments_without_copying_and_joins_once_when_a_read_needs_them() {
        let mut buffer = SessionBuffer::new();
        let segment = bytes::Bytes::from(vec![0x41u8; 4096]);
        let (calls, _) = account_copies(&mut buffer, |buffer| {
            for _ in 0..512 {
                buffer.push(segment.clone());
            }
        });
        assert_eq!(calls, 0);
        assert_eq!(buffer.len(), 512 * 4096);

        let (calls, copied) = account_copies(&mut buffer, |buffer| assert_eq!(buffer.decode(0, 8), "AAAAAAAA"));
        assert_eq!(calls, 1);
        assert_eq!(copied, 512 * 4096);
    }

    #[test]
    fn merges_the_run_that_reached_the_ceiling_not_everything_held() {
        let mut buffer = SessionBuffer::new();
        let held = 1024 * 1024;
        buffer.push(vec![0x41u8; held]);
        buffer.decode(0, 1);
        let (calls, copied) = account_copies(&mut buffer, |buffer| {
            for _ in 0..SEGMENT_CEILING {
                push(buffer, "x");
            }
        });
        assert_eq!(calls, 1);
        assert_eq!(copied, SEGMENT_CEILING as u64);
        assert_eq!(buffer.len(), held + SEGMENT_CEILING);
        assert_eq!(buffer.decode(held, held + 4), "xxxx");
    }

    #[test]
    fn keeps_the_octets_moved_linear_across_repeated_ceilings() {
        let mut buffer = SessionBuffer::new();
        let total = SEGMENT_CEILING * 8;
        let (_, copied) = account_copies(&mut buffer, |buffer| {
            for _ in 0..total {
                push(buffer, "x");
            }
        });
        assert_eq!(copied, total as u64);
        assert_eq!(buffer.len(), total);
        assert_eq!(buffer.decode(total - 2, total), "xx");
    }

    #[test]
    fn scans_an_unterminated_command_line_without_joining() {
        let mut buffer = SessionBuffer::new();
        let segments = 1024;
        let (_, copied) = account_copies(&mut buffer, |buffer| {
            for _ in 0..segments {
                buffer.push(vec![0x41u8; 64]);
                assert_eq!(buffer.index_of_crlf(), None);
            }
        });
        assert_eq!(copied, 0);
        assert_eq!(buffer.len(), segments * 64);
    }

    #[test]
    fn resumes_the_walk_where_the_last_scan_stopped_not_at_the_head_of_the_list() {
        let mut buffer = SessionBuffer::new();
        let segments = SEGMENT_CEILING - 96;
        for _ in 0..segments {
            push(&mut buffer, "x");
            assert_eq!(buffer.index_of_crlf(), None);
        }
        // Walking from the head each time would be about segments² / 2 steps.
        assert!(buffer.cost().segment_steps < 4 * segments as u64);
        assert_eq!(buffer.len(), segments);
    }

    #[test]
    fn keeps_the_resume_point_on_the_entry_a_merged_run_became() {
        let mut buffer = SessionBuffer::new();
        let segments = SEGMENT_CEILING + 64;
        for _ in 0..segments {
            push(&mut buffer, "x");
            assert_eq!(buffer.index_of_crlf(), None);
        }
        push(&mut buffer, "\r\n");
        assert_eq!(buffer.index_of_crlf(), Some(segments));
        assert_eq!(buffer.decode(0, 3), "xxx");
    }

    #[test]
    fn resumes_from_the_shifted_position_after_a_consume_that_leaves_segments_held() {
        let mut buffer = SessionBuffer::new();
        push(&mut buffer, "A1 NOOP\r\nPREFIX");
        assert_eq!(read_line(&mut buffer).as_deref(), Some("A1 NOOP"));
        let segments = 512;
        for _ in 0..segments {
            push(&mut buffer, "x");
            assert_eq!(buffer.index_of_crlf(), None);
        }
        let tail = "yyyyyyyyyy";
        push(&mut buffer, tail);
        assert_eq!(buffer.index_of_crlf(), None);
        buffer.consume(3);
        push(&mut buffer, "\r\n");
        assert_eq!(buffer.index_of_crlf(), Some(3 + segments + tail.len()));
        assert_eq!(buffer.decode(0, 3), "FIX");
    }

    #[test]
    fn ignores_an_empty_segment() {
        let mut buffer = SessionBuffer::new();
        buffer.push(Vec::new());
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.index_of_crlf(), None);
    }

    #[test]
    fn finds_a_crlf_straddling_the_boundary_a_scan_stopped_at() {
        let mut buffer = SessionBuffer::new();
        push(&mut buffer, "A1 NO");
        assert_eq!(buffer.index_of_crlf(), None);
        push(&mut buffer, "OP\r");
        assert_eq!(buffer.index_of_crlf(), None);
        push(&mut buffer, "\nA2 NOOP\r\n");
        assert_eq!(read_line(&mut buffer).as_deref(), Some("A1 NOOP"));
        assert_eq!(read_line(&mut buffer).as_deref(), Some("A2 NOOP"));
        assert_eq!(buffer.len(), 0);
    }

    #[test]
    fn resumes_the_search_from_the_right_place_after_a_consume() {
        let mut buffer = SessionBuffer::new();
        let first = format!("A1 SEARCH SUBJECT {}", "x".repeat(64));
        push(&mut buffer, &first);
        assert_eq!(buffer.index_of_crlf(), None);
        push(&mut buffer, "\r\nA2 NOOP\r\n");
        assert_eq!(read_line(&mut buffer), Some(first));
        assert_eq!(read_line(&mut buffer).as_deref(), Some("A2 NOOP"));
        assert_eq!(buffer.len(), 0);
    }

    #[test]
    fn finds_a_crlf_straddling_two_segments() {
        let mut buffer = SessionBuffer::new();
        push(&mut buffer, "A1 NOOP\r");
        push(&mut buffer, "\nA2 NOOP\r\n");
        assert_eq!(read_line(&mut buffer).as_deref(), Some("A1 NOOP"));
        assert_eq!(read_line(&mut buffer).as_deref(), Some("A2 NOOP"));
        assert_eq!(read_line(&mut buffer), None);
        assert_eq!(buffer.len(), 0);
    }

    #[test]
    fn finds_a_crlf_straddling_the_joined_block_and_a_new_segment() {
        let mut buffer = SessionBuffer::new();
        push(&mut buffer, "A1 NOOP\r\nA2 NOOP\r");
        assert_eq!(read_line(&mut buffer).as_deref(), Some("A1 NOOP"));
        push(&mut buffer, "\nA3 NOOP\r\n");
        assert_eq!(read_line(&mut buffer).as_deref(), Some("A2 NOOP"));
        assert_eq!(read_line(&mut buffer).as_deref(), Some("A3 NOOP"));
        assert_eq!(buffer.len(), 0);
    }

    #[test]
    fn reassembles_a_payload_delivered_one_octet_at_a_time() {
        let mut buffer = SessionBuffer::new();
        let payload = "héllo wörld";
        for octet in payload.bytes() {
            buffer.push(vec![octet]);
        }
        assert_eq!(buffer.len(), payload.len());
        assert_eq!(buffer.decode(0, payload.len()), payload);
    }

    #[test]
    fn counts_octets_not_code_units() {
        let mut buffer = SessionBuffer::new();
        push(&mut buffer, "ééé");
        assert_eq!(buffer.len(), 6);
        assert_eq!(buffer.decode(0, 2), "é");
    }

    #[test]
    fn keeps_reads_correct_across_the_compaction_the_cursor_triggers() {
        let mut buffer = SessionBuffer::new();
        let lines: Vec<String> = (0..20).map(|i| format!("L{i:02}")).collect();
        push(&mut buffer, &lines.iter().map(|line| format!("{line}\r\n")).collect::<String>());
        let read: Vec<String> = lines.iter().map(|_| read_line(&mut buffer).unwrap()).collect();
        assert_eq!(read, lines);
        assert_eq!(buffer.len(), 0);
    }

    #[test]
    fn drops_unjoined_segments_a_consume_reaches_past() {
        let mut buffer = SessionBuffer::new();
        push(&mut buffer, "0123456789");
        push(&mut buffer, "abcdeA1 NOOP\r\n");
        buffer.consume(15);
        assert_eq!(buffer.len(), 9);
        assert_eq!(read_line(&mut buffer).as_deref(), Some("A1 NOOP"));
    }

    #[test]
    fn releases_the_block_a_large_payload_was_sliced_out_of() {
        let mut buffer = SessionBuffer::new();
        let payload = 4 * 1024 * 1024;
        buffer.push(vec![0x41u8; payload]);
        push(&mut buffer, " tail\r\n");
        assert_eq!(buffer.decode(0, 1), "A");
        buffer.consume(payload);
        assert!(buffer.retained_block_len() < 1024);
        assert_eq!(read_line(&mut buffer).as_deref(), Some(" tail"));
    }

    #[test]
    fn copies_no_more_than_the_octets_consumed_while_draining_in_small_reads() {
        let mut buffer = SessionBuffer::new();
        let line = "A1 NOOP\r\n";
        let total = 64 * 1024;
        push(&mut buffer, &line.repeat(total / line.len()));
        let (_, copied) = account_copies(&mut buffer, |buffer| while read_line(buffer).is_some() {});
        assert_eq!(buffer.len(), 0);
        assert!(copied <= 2 * total as u64);
    }

    #[test]
    fn forgets_everything_on_clear() {
        let mut buffer = SessionBuffer::new();
        push(&mut buffer, "A1 NOOP\r\n");
        buffer.decode(0, 2);
        push(&mut buffer, "A2 NOOP\r\n");
        buffer.clear();
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.index_of_crlf(), None);
    }

    #[test]
    fn streams_held_segments_out_one_at_a_time_without_copying_any() {
        let mut buffer = SessionBuffer::new();
        for segment in ["aaaa", "bbbb", "cccc"] {
            push(&mut buffer, segment);
        }
        let mut pieces: Vec<String> = Vec::new();
        let (_, copied) = account_copies(&mut buffer, |buffer| {
            while !buffer.is_empty() {
                let piece = buffer.head(1024);
                pieces.push(String::from_utf8(piece.to_vec()).unwrap());
                buffer.consume(piece.len());
            }
        });
        assert_eq!(pieces, ["aaaa", "bbbb", "cccc"]);
        assert_eq!(copied, 0);
    }

    #[test]
    fn stops_at_the_requested_length_and_leaves_the_rest_readable_as_a_command() {
        let mut buffer = SessionBuffer::new();
        push(&mut buffer, "payl");
        push(&mut buffer, "oad");
        push(&mut buffer, " tail\r\nNEXT");

        let first = buffer.head(5);
        assert_eq!(&first[..], b"payl");
        buffer.consume(first.len());
        let second = buffer.head(3);
        assert_eq!(&second[..], b"oad");
        buffer.consume(second.len());

        let end = buffer.index_of_crlf().unwrap();
        assert_eq!(buffer.decode(0, end), " tail");
        buffer.consume(end + 2);
        let rest = buffer.len();
        assert_eq!(buffer.decode(0, rest), "NEXT");
    }

    #[test]
    fn keeps_a_view_valid_after_the_octets_behind_it_are_consumed() {
        let mut buffer = SessionBuffer::new();
        push(&mut buffer, "hello world");
        let piece = buffer.head(5);
        buffer.consume(5);
        push(&mut buffer, "!!!!!");
        let rest = buffer.len();
        buffer.consume(rest);
        assert_eq!(&piece[..], b"hello");
    }
}

mod trace {
    use mail_server::imap::trace::{clip, imap_trace, redact_credentials, trace_enabled, trace_lines, Direction};

    // The Bun tests reload the module under each `IMAP_TRACE` setting and spy
    // on the logger. Here the setting is read once per process, so the lines
    // the trace would journal are asserted through `trace_lines`.

    #[test]
    fn the_switch_follows_imap_trace_and_a_trace_call_is_safe_either_way() {
        assert_eq!(trace_enabled(), std::env::var("IMAP_TRACE").as_deref() == Ok("1"));
        imap_trace(Direction::In, "session_abc", "A1 NOOP");
    }

    #[test]
    fn records_one_line_per_non_empty_crlf_split_inbound_line() {
        assert_eq!(trace_lines(Direction::In, "A1 NOOP\r\nA2 CAPABILITY\r\n"), ["A1 NOOP", "A2 CAPABILITY"]);
    }

    #[test]
    fn redacts_login_password_on_inbound() {
        let lines = trace_lines(Direction::In, "A1 LOGIN \"admin\" \"hunter2\"");
        assert_eq!(lines, ["A1 LOGIN [REDACTED]"]);
        assert!(!lines[0].contains("hunter2"));
    }

    #[test]
    fn drops_literal_plus_continuation_lines_on_inbound() {
        // `admin {8+}` and `password` reach the trace as their own inbound
        // lines; neither is `<tag> <VERB>`, so neither is journalled.
        let lines = trace_lines(Direction::In, "A1 LOGIN {5+}\r\nadmin {8+}\r\npassword");
        assert_eq!(lines, ["A1 LOGIN [REDACTED]"]);
        assert!(!lines.iter().any(|line| line.contains("password")));
    }

    #[test]
    fn redacts_authenticate_initial_response_on_inbound() {
        let lines = trace_lines(Direction::In, "A2 AUTHENTICATE PLAIN AGFkbWluAGh1bnRlcjI=");
        assert_eq!(lines, ["A2 AUTHENTICATE PLAIN [REDACTED]"]);
        assert!(!lines[0].contains("AGFkbWlu"));
    }

    #[test]
    fn records_outbound_framing_lines_that_match_the_allowlist() {
        let data = [
            "* OK IMAP4rev1 Service Ready",
            "* CAPABILITY IMAP4rev1 LITERAL+",
            "* 12 EXISTS",
            "* 0 RECENT",
            "* BYE Server logging out",
            "+ go ahead",
            "A1 OK LOGIN completed",
            "A2 NO Not authenticated.",
            "A3 BAD Invalid syntax",
        ]
        .join("\r\n");
        assert_eq!(trace_lines(Direction::Out, &data).len(), 9);
    }

    #[test]
    fn drops_untagged_fetch_data_literal_payload_and_envelope_content() {
        let data = [
            "* 5 FETCH (UID 100 RFC822.SIZE 19222751 FLAGS (\\Seen))",
            "Subject: Attorney client privileged — do not disclose",
            "From: someone@example.com",
            "ENVELOPE (\"Wed\" \"hello\" ((NIL NIL \"alice\" \"ex.com\")) …)",
            ")",
        ]
        .join("\r\n");
        assert!(trace_lines(Direction::Out, &data).is_empty());
    }

    #[test]
    fn clips_lines_longer_than_the_cap_and_marks_the_overflow() {
        let lines = trace_lines(Direction::In, &format!("A1 SELECT {}", "X".repeat(600)));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].chars().count() < 600 + "A1 SELECT ".len());
        let marker = lines[0].rsplit_once("…[+").expect("an overflow marker").1;
        assert!(marker.ends_with(']') && marker[..marker.len() - 1].bytes().all(|b| b.is_ascii_digit()));
        assert_eq!(clip(&"x".repeat(512)), "x".repeat(512));
    }

    #[test]
    fn redacts_a_login_and_an_authenticate_the_caller_did_not_pre_trim() {
        // The handler redacts the raw assembled command text, which is not
        // trimmed, before it reaches the parse-failure log.
        assert_eq!(redact_credentials("  A1 LOGIN admin hunter2"), "  A1 LOGIN [REDACTED]");
        assert_eq!(redact_credentials("\r\n LOGIN admin hunter2"), "\r\n LOGIN [REDACTED]");
        assert_eq!(
            redact_credentials("  A1 AUTHENTICATE PLAIN AGFkbWluAGh1bnRlcjI="),
            "  A1 AUTHENTICATE PLAIN [REDACTED]"
        );
    }
}
