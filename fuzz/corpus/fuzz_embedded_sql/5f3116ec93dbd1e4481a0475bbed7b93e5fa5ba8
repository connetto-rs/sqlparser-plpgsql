
BEGIN
    IF NEW.event_type = 'error' THEN
        INSERT INTO audit_log (action, severity) VALUES ('error_event', 'high');
    ELSIF NEW.event_type = 'warning' THEN
        INSERT INTO audit_log (action, severity) VALUES ('warning_event', 'medium');
    ELSIF NEW.event_type = 'info' THEN
        INSERT INTO audit_log (action, severity) VALUES ('info_event', 'low');
    ELSE
        INSERT INTO audit_log (action, severity) VALUES ('unknown_event', 'unknown');
    END IF;
    RETURN NEW;
END;
